//! Daemon-side multiplexed credential tunnel handler.
//!
//! Handles a persistent TCP connection from a single agent, multiplexing
//! multiple credential proxy requests over frame-based messages. Each
//! request is identified by a `u32` request ID in the frame header.
//!
//! See `docs/specs/credential-protection.md` for the wire protocol spec.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::CellaDaemonError;
use crate::credential_proxy;
use crate::credential_resolver::{self, ProviderMeta};
use crate::phantom_registry::PhantomRegistry;

// ---------------------------------------------------------------------------
// Frame types
// ---------------------------------------------------------------------------

pub(crate) const FRAME_HANDSHAKE: u8 = 0x01;
pub(crate) const FRAME_REQUEST_ENVELOPE: u8 = 0x02;
pub(crate) const FRAME_REQUEST_CHUNK: u8 = 0x03;
pub(crate) const FRAME_REQUEST_END: u8 = 0x04;
pub(crate) const FRAME_RESPONSE_ENVELOPE: u8 = 0x05;
pub(crate) const FRAME_RESPONSE_CHUNK: u8 = 0x06;
pub(crate) const FRAME_RESPONSE_END: u8 = 0x07;
pub(crate) const FRAME_ERROR: u8 = 0x08;
pub(crate) const FRAME_CANCEL: u8 = 0x09;

// ---------------------------------------------------------------------------
// Resource limits (compile-time constants per spec)
// ---------------------------------------------------------------------------

const MAX_HANDSHAKE_PAYLOAD: u32 = 8 * 1024;
const MAX_ENVELOPE_PAYLOAD: u32 = 8 * 1024 * 1024;
const MAX_REQUEST_CHUNK: u32 = 16 * 1024 * 1024;
const MAX_ERROR_PAYLOAD: u32 = 8 * 1024;
const MAX_REQUEST_BODY: u64 = 256 * 1024 * 1024;
const MAX_CONCURRENT_REQUESTS: usize = 64;
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(5);

/// Frame header size: `request_id`(4) + `frame_type`(1) + `payload_len`(4).
const FRAME_HEADER_SIZE: usize = 9;

// ---------------------------------------------------------------------------
// Per-request state machine
// ---------------------------------------------------------------------------

/// States a single multiplexed request can be in.
///
/// The spec defines a `Responding` phase between `AwaitingResponse` and
/// `Terminal`. Here, response streaming is handled entirely within the
/// spawned upstream task, so `Responding` is collapsed into `AwaitingResponse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestPhase {
    Init,
    Handshaken,
    EnvelopeReceived,
    Streaming,
    AwaitingResponse,
    Terminal,
}

/// Tracks state for a single in-flight request.
struct RequestState {
    phase: RequestPhase,
    handshake: Option<cella_protocol::CredentialProxyHandshake>,
    envelope: Option<MuxRequestEnvelope>,
    body_chunks: Vec<u8>,
    body_total: u64,
    /// Handle to the spawned upstream task, used for cancellation.
    task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl RequestState {
    const fn new() -> Self {
        Self {
            phase: RequestPhase::Init,
            handshake: None,
            envelope: None,
            body_chunks: Vec::new(),
            body_total: 0,
            task_handle: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Envelope types (distinct from credential_proxy — no body_len field)
// ---------------------------------------------------------------------------

/// HTTP request metadata for the multiplexed credential tunnel.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct MuxRequestEnvelope {
    pub method: String,
    pub uri: String,
    pub headers: Vec<(String, String)>,
}

/// HTTP response metadata sent back through the tunnel.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct MuxResponseEnvelope {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

/// Error payload for ERROR frames.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct ErrorEnvelope {
    pub category: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Frame I/O helpers
// ---------------------------------------------------------------------------

/// Read a 9-byte frame header: `(request_id, frame_type, payload_len)`.
pub(crate) async fn read_frame_header<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> io::Result<(u32, u8, u32)> {
    let mut buf = [0u8; FRAME_HEADER_SIZE];
    reader.read_exact(&mut buf).await?;
    let request_id = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let frame_type = buf[4];
    let payload_len = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]);
    Ok((request_id, frame_type, payload_len))
}

/// Write a complete frame (header + payload).
pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    request_id: u32,
    frame_type: u8,
    payload: &[u8],
) -> io::Result<()> {
    let payload_len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    let mut header = [0u8; FRAME_HEADER_SIZE];
    header[..4].copy_from_slice(&request_id.to_be_bytes());
    header[4] = frame_type;
    header[5..9].copy_from_slice(&payload_len.to_be_bytes());
    writer.write_all(&header).await?;
    if !payload.is_empty() {
        writer.write_all(payload).await?;
    }
    writer.flush().await
}

// ---------------------------------------------------------------------------
// Shared writer wrapper
// ---------------------------------------------------------------------------

type SharedWriter<W> = Arc<Mutex<W>>;

async fn write_error_frame<W: AsyncWrite + Unpin>(
    writer: &SharedWriter<W>,
    request_id: u32,
    category: &str,
    message: &str,
    trace_id: Option<&str>,
) {
    let envelope = ErrorEnvelope {
        category: category.to_string(),
        message: message.to_string(),
        trace_id: trace_id.map(str::to_string),
    };
    let payload = serde_json::to_vec(&envelope).unwrap_or_default();
    let _ = write_frame(&mut *writer.lock().await, request_id, FRAME_ERROR, &payload).await;
}

// ---------------------------------------------------------------------------
// Main connection handler
// ---------------------------------------------------------------------------

/// Handle a single persistent multiplexed credential connection.
///
/// Called after the `0x03` magic byte has been consumed from the stream.
///
/// # Errors
///
/// Returns error on fatal I/O failures. Per-request errors are sent as
/// ERROR frames without closing the connection.
pub async fn handle_credential_connection(
    stream: tokio::net::TcpStream,
    phantom_registry: Arc<Mutex<PhantomRegistry>>,
) -> Result<(), CellaDaemonError> {
    let (reader, writer) = tokio::io::split(stream);
    let writer: SharedWriter<_> = Arc::new(Mutex::new(writer));
    let in_flight = Arc::new(AtomicUsize::new(0));

    run_frame_loop(reader, writer, phantom_registry, in_flight).await
}

async fn run_frame_loop<R: AsyncRead + Unpin, W: AsyncWrite + Unpin + Send + 'static>(
    mut reader: R,
    writer: SharedWriter<W>,
    phantom_registry: Arc<Mutex<PhantomRegistry>>,
    in_flight: Arc<AtomicUsize>,
) -> Result<(), CellaDaemonError> {
    let mut requests: HashMap<u32, RequestState> = HashMap::new();
    let mut last_activity = tokio::time::Instant::now();

    loop {
        let header = tokio::select! {
            result = read_frame_header(&mut reader) => {
                match result {
                    Ok(h) => h,
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                    Err(e) => return Err(CellaDaemonError::Socket {
                        message: format!("credential mux: read frame header: {e}"),
                    }),
                }
            }
            () = tokio::time::sleep_until(last_activity + IDLE_TIMEOUT) => {
                if in_flight.load(Ordering::Relaxed) == 0 {
                    debug!("Credential mux: idle timeout, closing connection");
                    break;
                }
                // Requests still in flight — reset timer to avoid busy-loop.
                last_activity = tokio::time::Instant::now();
                continue;
            }
        };

        last_activity = tokio::time::Instant::now();
        let (request_id, frame_type, payload_len) = header;

        if let Err(max) = validate_payload_len(frame_type, payload_len) {
            let tid = trace_id_for(&requests, request_id);
            send_protocol_violation(
                &writer,
                request_id,
                &format!("payload_len {payload_len} exceeds limit {max} for frame type 0x{frame_type:02x}"),
                tid.as_deref(),
            ).await;
            drain_payload(&mut reader, payload_len).await?;
            continue;
        }

        let payload = read_payload(&mut reader, payload_len).await?;

        dispatch_frame(
            request_id,
            frame_type,
            &payload,
            &mut requests,
            &writer,
            &phantom_registry,
            &in_flight,
        )
        .await;

        requests.retain(|_, state| {
            state.phase != RequestPhase::Terminal
                || state.task_handle.as_ref().is_some_and(|h| !h.is_finished())
        });
    }

    abort_in_flight(&mut requests);
    Ok(())
}

// ---------------------------------------------------------------------------
// Payload validation & reading
// ---------------------------------------------------------------------------

/// Validate payload length against per-frame-type limits. Returns
/// `Err(max)` if the payload exceeds the limit.
const fn validate_payload_len(frame_type: u8, payload_len: u32) -> Result<(), u32> {
    let max = match frame_type {
        FRAME_REQUEST_ENVELOPE | FRAME_RESPONSE_ENVELOPE => MAX_ENVELOPE_PAYLOAD,
        FRAME_REQUEST_CHUNK | FRAME_RESPONSE_CHUNK => MAX_REQUEST_CHUNK,
        FRAME_REQUEST_END | FRAME_CANCEL | FRAME_RESPONSE_END => 0,
        FRAME_ERROR => MAX_ERROR_PAYLOAD,
        // FRAME_HANDSHAKE and unknown types get a tight 8 KB cap
        _ => MAX_HANDSHAKE_PAYLOAD,
    };
    if payload_len > max { Err(max) } else { Ok(()) }
}

async fn read_payload<R: AsyncRead + Unpin>(
    reader: &mut R,
    len: u32,
) -> Result<Vec<u8>, CellaDaemonError> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let size = usize::try_from(len).unwrap_or(usize::MAX);
    let mut buf = vec![0u8; size];
    reader
        .read_exact(&mut buf)
        .await
        .map_err(|e| CellaDaemonError::Socket {
            message: format!("credential mux: read payload: {e}"),
        })?;
    Ok(buf)
}

async fn drain_payload<R: AsyncRead + Unpin>(
    reader: &mut R,
    len: u32,
) -> Result<(), CellaDaemonError> {
    if len == 0 {
        return Ok(());
    }
    let mut remaining = u64::from(len);
    let mut buf = [0u8; 8192];
    while remaining > 0 {
        let to_read = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        reader
            .read_exact(&mut buf[..to_read])
            .await
            .map_err(|e| CellaDaemonError::Socket {
                message: format!("credential mux: drain payload: {e}"),
            })?;
        remaining -= to_read as u64;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Frame dispatch
// ---------------------------------------------------------------------------

async fn dispatch_frame<W: AsyncWrite + Unpin + Send + 'static>(
    request_id: u32,
    frame_type: u8,
    payload: &[u8],
    requests: &mut HashMap<u32, RequestState>,
    writer: &SharedWriter<W>,
    phantom_registry: &Arc<Mutex<PhantomRegistry>>,
    in_flight: &Arc<AtomicUsize>,
) {
    match frame_type {
        FRAME_HANDSHAKE => {
            process_handshake(
                request_id,
                payload,
                requests,
                writer,
                phantom_registry,
                in_flight,
            )
            .await;
        }
        FRAME_REQUEST_ENVELOPE => {
            process_envelope(
                request_id,
                payload,
                requests,
                writer,
                phantom_registry,
                in_flight,
            )
            .await;
        }
        FRAME_REQUEST_CHUNK => {
            process_chunk(request_id, payload, requests, writer, in_flight).await;
        }
        FRAME_REQUEST_END => {
            process_request_end(request_id, requests, writer, phantom_registry, in_flight).await;
        }
        FRAME_CANCEL => {
            process_cancel(request_id, requests, in_flight);
        }
        _ => {
            let tid = trace_id_for(requests, request_id);
            send_protocol_violation(
                writer,
                request_id,
                &format!("unknown frame type 0x{frame_type:02x}"),
                tid.as_deref(),
            )
            .await;
        }
    }
}

// ---------------------------------------------------------------------------
// HANDSHAKE processing
// ---------------------------------------------------------------------------

async fn process_handshake<W: AsyncWrite + Unpin>(
    request_id: u32,
    payload: &[u8],
    requests: &mut HashMap<u32, RequestState>,
    writer: &SharedWriter<W>,
    phantom_registry: &Arc<Mutex<PhantomRegistry>>,
    in_flight: &Arc<AtomicUsize>,
) {
    // Reject duplicate active request IDs
    if requests.contains_key(&request_id) {
        let tid = trace_id_for(requests, request_id);
        send_protocol_violation(
            writer,
            request_id,
            "duplicate active request_id",
            tid.as_deref(),
        )
        .await;
        return;
    }

    // Concurrency limit
    if in_flight.load(Ordering::Relaxed) >= MAX_CONCURRENT_REQUESTS {
        write_error_frame(
            writer,
            request_id,
            "too_many_requests",
            "Concurrent request limit exceeded",
            None,
        )
        .await;
        return;
    }

    let handshake: cella_protocol::CredentialProxyHandshake = match serde_json::from_slice(payload)
    {
        Ok(hs) => hs,
        Err(e) => {
            send_protocol_violation(
                writer,
                request_id,
                &format!("invalid handshake payload: {e}"),
                None,
            )
            .await;
            return;
        }
    };

    let trace = handshake.trace_id.as_deref();

    // Nonce is mandatory for the mux protocol
    let Some(nonce) = &handshake.container_nonce else {
        write_error_frame(
            writer,
            request_id,
            "nonce_invalid",
            "container_nonce is required",
            trace,
        )
        .await;
        return;
    };

    if !phantom_registry
        .lock()
        .await
        .validate_nonce(&handshake.container_name, nonce)
    {
        warn!(
            "Credential mux: invalid nonce for container {}",
            handshake.container_name
        );
        write_error_frame(
            writer,
            request_id,
            "nonce_invalid",
            "Container nonce does not match registered nonce",
            trace,
        )
        .await;
        return;
    }

    in_flight.fetch_add(1, Ordering::Relaxed);
    let mut state = RequestState::new();
    state.phase = RequestPhase::Handshaken;
    state.handshake = Some(handshake);
    requests.insert(request_id, state);

    debug!("Credential mux: request {request_id} handshake accepted");
}

// ---------------------------------------------------------------------------
// REQUEST_ENVELOPE processing
// ---------------------------------------------------------------------------

async fn process_envelope<W: AsyncWrite + Unpin>(
    request_id: u32,
    payload: &[u8],
    requests: &mut HashMap<u32, RequestState>,
    writer: &SharedWriter<W>,
    phantom_registry: &Arc<Mutex<PhantomRegistry>>,
    in_flight: &Arc<AtomicUsize>,
) {
    let tid = trace_id_for(requests, request_id);

    let Some(state) = requests.get_mut(&request_id) else {
        send_protocol_violation(
            writer,
            request_id,
            "no active request for this request_id",
            tid.as_deref(),
        )
        .await;
        return;
    };

    if state.phase != RequestPhase::Handshaken {
        let msg = format!("unexpected REQUEST_ENVELOPE in state {:?}", state.phase);
        send_protocol_violation(writer, request_id, &msg, tid.as_deref()).await;
        mark_terminal(state, in_flight);
        return;
    }

    let envelope: MuxRequestEnvelope = match serde_json::from_slice(payload) {
        Ok(e) => e,
        Err(e) => {
            send_protocol_violation(
                writer,
                request_id,
                &format!("invalid request envelope: {e}"),
                tid.as_deref(),
            )
            .await;
            mark_terminal(state, in_flight);
            return;
        }
    };

    if let Err(reason) = credential_proxy::validate_uri(&envelope.uri) {
        send_protocol_violation(
            writer,
            request_id,
            &format!("invalid URI '{}': {reason}", envelope.uri),
            tid.as_deref(),
        )
        .await;
        mark_terminal(state, in_flight);
        return;
    }

    // Validate phantom token + provider + domain
    let handshake = state
        .handshake
        .as_ref()
        .expect("handshake set in Handshaken");
    if let Err((cat, msg)) =
        validate_token_and_provider(handshake, &envelope, phantom_registry).await
    {
        write_error_frame(writer, request_id, &cat, &msg, tid.as_deref()).await;
        mark_terminal(state, in_flight);
        return;
    }

    state.envelope = Some(envelope);
    state.phase = RequestPhase::EnvelopeReceived;
}

async fn validate_token_and_provider(
    handshake: &cella_protocol::CredentialProxyHandshake,
    envelope: &MuxRequestEnvelope,
    phantom_registry: &Arc<Mutex<PhantomRegistry>>,
) -> Result<(), (String, String)> {
    let registry = phantom_registry.lock().await;

    let header_name = registry
        .get_provider_meta(&handshake.container_name, &handshake.provider_id)
        .map_or_else(
            || {
                cella_env::credential_providers::CREDENTIAL_PROVIDERS
                    .iter()
                    .find(|p| p.id == handshake.provider_id)
                    .map_or_else(|| "Authorization".to_string(), |p| p.header.to_string())
            },
            |m| m.header.clone(),
        );

    let phantom_token = credential_proxy::extract_phantom_token(&envelope.headers, &header_name)
        .ok_or_else(|| {
            (
                "token_invalid".to_string(),
                format!("No phantom token found in {header_name} header"),
            )
        })?;

    let provider_id = registry
        .lookup(&handshake.container_name, &phantom_token)
        .map(String::from)
        .ok_or_else(|| {
            (
                "token_invalid".to_string(),
                format!(
                    "Phantom token not found in registry for container {}",
                    handshake.container_name
                ),
            )
        })?;

    if provider_id != handshake.provider_id {
        return Err((
            "provider_mismatch".to_string(),
            format!(
                "Handshake provider {} does not match token provider {provider_id}",
                handshake.provider_id,
            ),
        ));
    }

    let domains: Vec<String> = registry
        .provider_domains(&handshake.container_name, &provider_id)
        .map(<[String]>::to_vec)
        .ok_or_else(|| {
            (
                "domain_unregistered".to_string(),
                format!("No registered domains for provider {provider_id}"),
            )
        })?;
    drop(registry);

    if !domains.iter().any(|d| d == &handshake.domain) {
        return Err((
            "domain_unregistered".to_string(),
            format!(
                "Domain {} not registered for provider {provider_id}",
                handshake.domain,
            ),
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// REQUEST_CHUNK processing
// ---------------------------------------------------------------------------

async fn process_chunk<W: AsyncWrite + Unpin>(
    request_id: u32,
    payload: &[u8],
    requests: &mut HashMap<u32, RequestState>,
    writer: &SharedWriter<W>,
    in_flight: &Arc<AtomicUsize>,
) {
    let tid = trace_id_for(requests, request_id);

    let Some(state) = requests.get_mut(&request_id) else {
        send_protocol_violation(
            writer,
            request_id,
            "no active request for this request_id",
            tid.as_deref(),
        )
        .await;
        return;
    };

    if state.phase != RequestPhase::EnvelopeReceived && state.phase != RequestPhase::Streaming {
        let msg = format!("unexpected REQUEST_CHUNK in state {:?}", state.phase);
        send_protocol_violation(writer, request_id, &msg, tid.as_deref()).await;
        mark_terminal(state, in_flight);
        return;
    }

    state.body_total += payload.len() as u64; // truncation impossible: payload bounded by u32 max
    if state.body_total > MAX_REQUEST_BODY {
        write_error_frame(
            writer,
            request_id,
            "body_exceeded",
            &format!("Request body exceeds {MAX_REQUEST_BODY} bytes"),
            tid.as_deref(),
        )
        .await;
        mark_terminal(state, in_flight);
        return;
    }

    state.body_chunks.extend_from_slice(payload);
    state.phase = RequestPhase::Streaming;
}

// ---------------------------------------------------------------------------
// REQUEST_END processing
// ---------------------------------------------------------------------------

async fn process_request_end<W: AsyncWrite + Unpin + Send + 'static>(
    request_id: u32,
    requests: &mut HashMap<u32, RequestState>,
    writer: &SharedWriter<W>,
    phantom_registry: &Arc<Mutex<PhantomRegistry>>,
    in_flight: &Arc<AtomicUsize>,
) {
    let tid = trace_id_for(requests, request_id);

    let Some(state) = requests.get_mut(&request_id) else {
        send_protocol_violation(
            writer,
            request_id,
            "no active request for this request_id",
            tid.as_deref(),
        )
        .await;
        return;
    };

    let valid = matches!(
        state.phase,
        RequestPhase::EnvelopeReceived | RequestPhase::Streaming
    );
    if !valid {
        let msg = format!("unexpected REQUEST_END in state {:?}", state.phase);
        send_protocol_violation(writer, request_id, &msg, tid.as_deref()).await;
        mark_terminal(state, in_flight);
        return;
    }

    state.phase = RequestPhase::AwaitingResponse;

    // Extract everything needed for the upstream task
    let handshake = state.handshake.clone().expect("handshake present");
    let envelope = state.envelope.take().expect("envelope present");
    let body = std::mem::take(&mut state.body_chunks);
    let writer_clone = writer.clone();
    let registry_clone = phantom_registry.clone();
    let in_flight_clone = in_flight.clone();

    let handle = tokio::spawn(async move {
        let _guard = InFlightGuard::new(in_flight_clone);
        execute_upstream(
            request_id,
            &handshake,
            &envelope,
            &body,
            &writer_clone,
            &registry_clone,
        )
        .await;
    });

    state.task_handle = Some(handle);
}

/// RAII guard that decrements the in-flight counter on drop.
struct InFlightGuard {
    counter: Arc<AtomicUsize>,
}

impl InFlightGuard {
    const fn new(counter: Arc<AtomicUsize>) -> Self {
        Self { counter }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Upstream execution (runs in a spawned task)
// ---------------------------------------------------------------------------

async fn execute_upstream<W: AsyncWrite + Unpin>(
    request_id: u32,
    handshake: &cella_protocol::CredentialProxyHandshake,
    envelope: &MuxRequestEnvelope,
    body: &[u8],
    writer: &SharedWriter<W>,
    phantom_registry: &Arc<Mutex<PhantomRegistry>>,
) {
    let trace = handshake.trace_id.as_deref();

    let Some(cred) = resolve_for_request(handshake, phantom_registry).await else {
        write_error_frame(
            writer,
            request_id,
            "credential_unavailable",
            &format!(
                "Cannot resolve credential for provider {}",
                handshake.provider_id
            ),
            trace,
        )
        .await;
        return;
    };

    let proxy_envelope = credential_proxy::HttpRequestEnvelope {
        method: envelope.method.clone(),
        uri: envelope.uri.clone(),
        headers: envelope.headers.clone(),
        body_len: u32::try_from(body.len()).unwrap_or(u32::MAX),
    };

    let result =
        credential_proxy::make_upstream_request(&proxy_envelope, body, &handshake.domain, &cred)
            .await;

    match result {
        Ok(resp) => stream_response(request_id, resp, writer).await,
        Err(e) => {
            write_error_frame(
                writer,
                request_id,
                "upstream_error",
                &format!("Upstream request failed: {e}"),
                trace,
            )
            .await;
        }
    }

    info!(
        "CRED_MUX {} {} {} {}",
        request_id, handshake.container_name, handshake.provider_id, envelope.uri,
    );
}

async fn resolve_for_request(
    handshake: &cella_protocol::CredentialProxyHandshake,
    phantom_registry: &Arc<Mutex<PhantomRegistry>>,
) -> Option<credential_resolver::ResolvedCredential> {
    let stored_meta = phantom_registry
        .lock()
        .await
        .get_provider_meta(&handshake.container_name, &handshake.provider_id)
        .cloned();

    let meta = stored_meta.as_ref().map_or_else(
        || ProviderMeta {
            env_var: format!("{}_API_KEY", handshake.provider_id.to_uppercase()),
            header: "Authorization".to_string(),
            prefix: String::new(),
        },
        |m| ProviderMeta {
            env_var: m.env_var.clone(),
            header: m.header.clone(),
            prefix: m.prefix.clone(),
        },
    );

    let hostname = stored_meta
        .as_ref()
        .and_then(|m| m.domains.first())
        .cloned()
        .unwrap_or_else(|| handshake.domain.clone());

    credential_resolver::resolve_credential(&handshake.provider_id, &meta, &hostname).await
}

async fn stream_response<W: AsyncWrite + Unpin>(
    request_id: u32,
    resp: reqwest::Response,
    writer: &SharedWriter<W>,
) {
    use futures_util::StreamExt;

    let status = resp.status().as_u16();
    let resp_headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    let resp_envelope = MuxResponseEnvelope {
        status,
        headers: resp_headers,
    };
    let payload = serde_json::to_vec(&resp_envelope).unwrap_or_default();
    let _ = write_frame(
        &mut *writer.lock().await,
        request_id,
        FRAME_RESPONSE_ENVELOPE,
        &payload,
    )
    .await;

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                let _ = write_frame(
                    &mut *writer.lock().await,
                    request_id,
                    FRAME_RESPONSE_CHUNK,
                    &bytes,
                )
                .await;
            }
            Err(e) => {
                warn!("Credential mux: upstream read error for request {request_id}: {e}");
                break;
            }
        }
    }

    let _ = write_frame(
        &mut *writer.lock().await,
        request_id,
        FRAME_RESPONSE_END,
        &[],
    )
    .await;
}

// ---------------------------------------------------------------------------
// CANCEL processing
// ---------------------------------------------------------------------------

fn process_cancel(
    request_id: u32,
    requests: &mut HashMap<u32, RequestState>,
    in_flight: &Arc<AtomicUsize>,
) {
    let Some(state) = requests.get_mut(&request_id) else {
        return; // Already terminal or never existed — no-op per spec
    };

    if state.phase == RequestPhase::Terminal {
        return; // RESPONSE_END already sent — CANCEL is a no-op
    }

    // If a task was spawned, its InFlightGuard handles the decrement on abort.
    // If no task was spawned yet, we decrement here.
    if let Some(handle) = state.task_handle.take() {
        handle.abort();
    } else {
        in_flight.fetch_sub(1, Ordering::Relaxed);
    }

    // Set Terminal directly — we already handled the in_flight decrement above.
    state.phase = RequestPhase::Terminal;
    debug!("Credential mux: request {request_id} cancelled");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Transition a request to Terminal state.
///
/// If the request was counted in `in_flight` but no upstream task was
/// spawned (so no `InFlightGuard` will handle the decrement), this
/// decrements the counter to prevent counter leaks.
fn mark_terminal(state: &mut RequestState, in_flight: &Arc<AtomicUsize>) {
    if state.phase == RequestPhase::Terminal {
        return;
    }
    // If no task was spawned, we own the in-flight count for this request.
    if state.task_handle.is_none() && state.phase != RequestPhase::Init {
        in_flight.fetch_sub(1, Ordering::Relaxed);
    }
    state.phase = RequestPhase::Terminal;
}

fn trace_id_for(requests: &HashMap<u32, RequestState>, request_id: u32) -> Option<String> {
    requests
        .get(&request_id)
        .and_then(|s| s.handshake.as_ref())
        .and_then(|hs| hs.trace_id.clone())
}

async fn send_protocol_violation<W: AsyncWrite + Unpin>(
    writer: &SharedWriter<W>,
    request_id: u32,
    message: &str,
    trace_id: Option<&str>,
) {
    write_error_frame(writer, request_id, "protocol_violation", message, trace_id).await;
}

fn abort_in_flight(requests: &mut HashMap<u32, RequestState>) {
    for (_, state) in requests.iter_mut() {
        if let Some(handle) = state.task_handle.take() {
            handle.abort();
        }
    }
    requests.clear();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Frame header roundtrip --

    #[tokio::test]
    async fn frame_header_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 42, FRAME_HANDSHAKE, b"hello")
            .await
            .unwrap();

        let mut cursor = io::Cursor::new(&buf);
        let (rid, ft, plen) = read_frame_header(&mut cursor).await.unwrap();
        assert_eq!(rid, 42);
        assert_eq!(ft, FRAME_HANDSHAKE);
        assert_eq!(plen, 5);

        let mut payload = vec![0u8; plen as usize];
        cursor.read_exact(&mut payload).await.unwrap();
        assert_eq!(payload, b"hello");
    }

    #[tokio::test]
    async fn frame_empty_payload_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 0, FRAME_REQUEST_END, &[])
            .await
            .unwrap();

        let mut cursor = io::Cursor::new(&buf);
        let (rid, ft, plen) = read_frame_header(&mut cursor).await.unwrap();
        assert_eq!(rid, 0);
        assert_eq!(ft, FRAME_REQUEST_END);
        assert_eq!(plen, 0);
    }

    #[tokio::test]
    async fn frame_max_request_id() {
        let mut buf = Vec::new();
        write_frame(&mut buf, u32::MAX, FRAME_CANCEL, &[])
            .await
            .unwrap();

        let mut cursor = io::Cursor::new(&buf);
        let (rid, ft, plen) = read_frame_header(&mut cursor).await.unwrap();
        assert_eq!(rid, u32::MAX);
        assert_eq!(ft, FRAME_CANCEL);
        assert_eq!(plen, 0);
    }

    // -- State machine transitions --

    #[test]
    fn request_state_starts_at_init() {
        let state = RequestState::new();
        assert_eq!(state.phase, RequestPhase::Init);
    }

    #[test]
    fn state_transitions_valid_path() {
        let mut state = RequestState::new();
        assert_eq!(state.phase, RequestPhase::Init);

        state.phase = RequestPhase::Handshaken;
        assert_eq!(state.phase, RequestPhase::Handshaken);

        state.phase = RequestPhase::EnvelopeReceived;
        assert_eq!(state.phase, RequestPhase::EnvelopeReceived);

        state.phase = RequestPhase::Streaming;
        assert_eq!(state.phase, RequestPhase::Streaming);

        state.phase = RequestPhase::AwaitingResponse;
        assert_eq!(state.phase, RequestPhase::AwaitingResponse);

        state.phase = RequestPhase::Terminal;
        assert_eq!(state.phase, RequestPhase::Terminal);
    }

    // -- Payload limit validation --

    #[test]
    fn validate_handshake_payload_limit() {
        assert!(validate_payload_len(FRAME_HANDSHAKE, MAX_HANDSHAKE_PAYLOAD).is_ok());
        assert!(validate_payload_len(FRAME_HANDSHAKE, MAX_HANDSHAKE_PAYLOAD + 1).is_err());
    }

    #[test]
    fn validate_envelope_payload_limit() {
        assert!(validate_payload_len(FRAME_REQUEST_ENVELOPE, MAX_ENVELOPE_PAYLOAD).is_ok());
        assert!(validate_payload_len(FRAME_REQUEST_ENVELOPE, MAX_ENVELOPE_PAYLOAD + 1).is_err());
    }

    #[test]
    fn validate_chunk_payload_limit() {
        assert!(validate_payload_len(FRAME_REQUEST_CHUNK, MAX_REQUEST_CHUNK).is_ok());
        assert!(validate_payload_len(FRAME_REQUEST_CHUNK, MAX_REQUEST_CHUNK + 1).is_err());
    }

    #[test]
    fn validate_end_and_cancel_must_be_empty() {
        assert!(validate_payload_len(FRAME_REQUEST_END, 0).is_ok());
        assert!(validate_payload_len(FRAME_REQUEST_END, 1).is_err());
        assert!(validate_payload_len(FRAME_CANCEL, 0).is_ok());
        assert!(validate_payload_len(FRAME_CANCEL, 1).is_err());
    }

    #[test]
    fn validate_unknown_frame_type_tight_cap() {
        assert!(validate_payload_len(0xFF, MAX_HANDSHAKE_PAYLOAD).is_ok());
        assert!(validate_payload_len(0xFF, MAX_HANDSHAKE_PAYLOAD + 1).is_err());
    }

    // -- Concurrency limit --

    #[tokio::test]
    async fn concurrency_limit_rejects_excess() {
        let (client, server) = tokio::io::duplex(65536);
        let (mut client_r, client_w) = tokio::io::split(client);
        let (server_r, server_w) = tokio::io::split(server);
        let writer: SharedWriter<_> = Arc::new(Mutex::new(server_w));
        let registry = Arc::new(Mutex::new(PhantomRegistry::new()));
        let in_flight = Arc::new(AtomicUsize::new(MAX_CONCURRENT_REQUESTS));

        // Register a container so nonce validation can work
        let entries = vec![cella_protocol::PhantomTokenEntry {
            provider_id: "test".to_string(),
            phantom_token: "pt-test".to_string(),
            env_var: "TEST_KEY".to_string(),
            domains: vec!["example.com".to_string()],
            header: "Authorization".to_string(),
            prefix: String::new(),
        }];
        let nonce = registry.lock().await.register("test-container", &entries);

        let hs = cella_protocol::CredentialProxyHandshake {
            auth_token: String::new(),
            container_name: "test-container".to_string(),
            request_id: "trace-1".to_string(),
            domain: "example.com".to_string(),
            provider_id: "test".to_string(),
            container_nonce: Some(nonce),
            trace_id: Some("trace-1".to_string()),
        };
        let payload = serde_json::to_vec(&hs).unwrap();

        let mut requests = HashMap::new();
        process_handshake(1, &payload, &mut requests, &writer, &registry, &in_flight).await;

        // Should have been rejected — no request state created
        assert!(!requests.contains_key(&1));

        // Read the error frame from the server side
        drop(writer);
        drop(server_r);
        let mut response = Vec::new();
        let _ = client_r.read_to_end(&mut response).await;

        // Verify it contains "too_many_requests"
        let response_str = String::from_utf8_lossy(&response);
        assert!(
            response_str.contains("too_many_requests"),
            "expected too_many_requests error, got: {response_str}"
        );

        drop(client_w);
    }

    // -- Unknown frame type rejection --

    #[tokio::test]
    async fn unknown_frame_type_sends_protocol_violation() {
        let (client, server) = tokio::io::duplex(65536);
        let (mut client_r, client_w) = tokio::io::split(client);
        let (server_r, server_w) = tokio::io::split(server);
        let writer: SharedWriter<_> = Arc::new(Mutex::new(server_w));
        let registry = Arc::new(Mutex::new(PhantomRegistry::new()));
        let in_flight = Arc::new(AtomicUsize::new(0));

        let mut requests = HashMap::new();
        dispatch_frame(1, 0xFE, &[], &mut requests, &writer, &registry, &in_flight).await;

        drop(writer);
        drop(server_r);
        let mut response = Vec::new();
        let _ = client_r.read_to_end(&mut response).await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(
            response_str.contains("protocol_violation"),
            "expected protocol_violation, got: {response_str}"
        );
        assert!(response_str.contains("unknown frame type"));

        drop(client_w);
    }

    // -- Invalid state transitions --

    #[tokio::test]
    async fn envelope_in_wrong_state_sends_protocol_violation() {
        let (client, server) = tokio::io::duplex(65536);
        let (mut client_r, client_w) = tokio::io::split(client);
        let (server_r, server_w) = tokio::io::split(server);
        let writer: SharedWriter<_> = Arc::new(Mutex::new(server_w));
        let registry = Arc::new(Mutex::new(PhantomRegistry::new()));
        let in_flight = Arc::new(AtomicUsize::new(1));

        // Insert a request in EnvelopeReceived state — wrong for another envelope
        let mut requests = HashMap::new();
        let mut state = RequestState::new();
        state.phase = RequestPhase::EnvelopeReceived;
        state.handshake = Some(cella_protocol::CredentialProxyHandshake {
            auth_token: String::new(),
            container_name: "c".to_string(),
            request_id: "r".to_string(),
            domain: "d".to_string(),
            provider_id: "p".to_string(),
            container_nonce: None,
            trace_id: None,
        });
        requests.insert(1, state);

        process_envelope(1, b"{}", &mut requests, &writer, &registry, &in_flight).await;

        // State should be terminal and in_flight decremented
        assert_eq!(requests.get(&1).unwrap().phase, RequestPhase::Terminal);
        assert_eq!(in_flight.load(Ordering::Relaxed), 0);

        drop(writer);
        drop(server_r);
        let mut response = Vec::new();
        let _ = client_r.read_to_end(&mut response).await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(
            response_str.contains("protocol_violation"),
            "expected protocol_violation, got: {response_str}"
        );

        drop(client_w);
    }

    // -- Envelope types roundtrip --

    #[test]
    fn mux_request_envelope_roundtrip() {
        let env = MuxRequestEnvelope {
            method: "POST".to_string(),
            uri: "/v1/messages".to_string(),
            headers: vec![("x-api-key".to_string(), "pt-abc".to_string())],
        };
        let json = serde_json::to_string(&env).unwrap();
        let decoded: MuxRequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.method, "POST");
        assert_eq!(decoded.uri, "/v1/messages");
        assert_eq!(decoded.headers.len(), 1);
    }

    #[test]
    fn mux_response_envelope_roundtrip() {
        let env = MuxResponseEnvelope {
            status: 200,
            headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
        };
        let json = serde_json::to_string(&env).unwrap();
        let decoded: MuxResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.status, 200);
    }

    #[test]
    fn error_envelope_roundtrip() {
        let env = ErrorEnvelope {
            category: "token_invalid".to_string(),
            message: "Phantom token not found".to_string(),
            trace_id: Some("cred-123".to_string()),
        };
        let json = serde_json::to_string(&env).unwrap();
        let decoded: ErrorEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.category, "token_invalid");
        assert_eq!(decoded.trace_id.as_deref(), Some("cred-123"));
    }

    #[test]
    fn error_envelope_no_trace_id() {
        let env = ErrorEnvelope {
            category: "nonce_invalid".to_string(),
            message: "bad nonce".to_string(),
            trace_id: None,
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("trace_id"));
    }

    // -- CANCEL cleans up properly --

    #[test]
    fn cancel_marks_terminal() {
        let in_flight = Arc::new(AtomicUsize::new(1));
        let mut requests = HashMap::new();
        let mut state = RequestState::new();
        state.phase = RequestPhase::EnvelopeReceived;
        requests.insert(1, state);

        process_cancel(1, &mut requests, &in_flight);

        assert_eq!(requests.get(&1).unwrap().phase, RequestPhase::Terminal);
        assert_eq!(in_flight.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn cancel_terminal_is_noop() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let mut requests = HashMap::new();
        let mut state = RequestState::new();
        state.phase = RequestPhase::Terminal;
        requests.insert(1, state);

        // Should not panic or decrement below zero
        process_cancel(1, &mut requests, &in_flight);
        assert_eq!(in_flight.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn cancel_nonexistent_is_noop() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let mut requests = HashMap::new();
        process_cancel(99, &mut requests, &in_flight);
    }
}
