//! Agent-side multiplexed credential tunnel client.
//!
//! Maintains a persistent TCP connection to the daemon using binary frame
//! protocol (magic byte `0x03`) for credential proxy requests.  Multiple
//! requests can be multiplexed over a single connection using unique
//! `request_id` values.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::proxy_config::CredentialRoute;

type BoxBody =
    http_body_util::combinators::BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

type BodyFrame = Result<hyper::body::Frame<Bytes>, Box<dyn std::error::Error + Send + Sync>>;

// -- Magic byte for credential proxy connections ------------------------------

const MAGIC_CREDENTIAL_PROXY: u8 = 0x03;

// -- Frame type constants -----------------------------------------------------

const FRAME_HANDSHAKE: u8 = 0x01;
const FRAME_REQUEST_ENVELOPE: u8 = 0x02;
const FRAME_REQUEST_CHUNK: u8 = 0x03;
const FRAME_REQUEST_END: u8 = 0x04;
const FRAME_RESPONSE_ENVELOPE: u8 = 0x05;
const FRAME_RESPONSE_CHUNK: u8 = 0x06;
const FRAME_RESPONSE_END: u8 = 0x07;
const FRAME_ERROR: u8 = 0x08;

// -- Limits -------------------------------------------------------------------

const FRAME_HEADER_SIZE: usize = 9;
const MAX_REQUEST_BODY: usize = 256 * 1024 * 1024;
const MAX_REQUEST_CHUNK: usize = 16 * 1024 * 1024;

// -- Wire types ---------------------------------------------------------------

/// Request envelope sent as `REQUEST_ENVELOPE` frame payload (JSON).
#[derive(Debug, serde::Serialize)]
struct MuxRequestEnvelope {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
}

/// Response envelope received as `RESPONSE_ENVELOPE` frame payload (JSON).
#[derive(Debug, serde::Deserialize)]
struct MuxResponseEnvelope {
    status: u16,
    headers: Vec<(String, String)>,
}

/// Error envelope received as `ERROR` frame payload (JSON).
#[derive(Debug, serde::Deserialize)]
struct ErrorEnvelope {
    category: String,
    message: String,
}

// -- Pending-response bookkeeping ---------------------------------------------

/// Data delivered from the reader task to a waiting request.
enum ResponseData {
    /// Successful envelope with a channel for body chunks.
    Envelope {
        envelope: MuxResponseEnvelope,
        body_rx: tokio::sync::mpsc::Receiver<BodyFrame>,
    },
    /// Error from the daemon (ERROR frame).
    Error { category: String, message: String },
}

/// Per-request state held by the reader task while assembling a response.
struct InFlightRequest {
    /// Sender for the final envelope or error.
    envelope_tx: Option<oneshot::Sender<ResponseData>>,
    /// Sender for streaming body chunks (created on `RESPONSE_ENVELOPE`).
    body_tx: Option<tokio::sync::mpsc::Sender<BodyFrame>>,
}

type PendingMap = Arc<Mutex<HashMap<u32, InFlightRequest>>>;

// -- Frame I/O helpers --------------------------------------------------------

async fn write_frame<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    request_id: u32,
    frame_type: u8,
    payload: &[u8],
) -> io::Result<()> {
    let payload_len =
        u32::try_from(payload.len()).map_err(|_| io::Error::other("payload too large"))?;
    writer.write_all(&request_id.to_be_bytes()).await?;
    writer.write_u8(frame_type).await?;
    writer.write_all(&payload_len.to_be_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await
}

async fn read_frame_header<R: AsyncReadExt + Unpin>(reader: &mut R) -> io::Result<(u32, u8, u32)> {
    let mut buf = [0u8; FRAME_HEADER_SIZE];
    reader.read_exact(&mut buf).await?;
    let request_id = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let frame_type = buf[4];
    let payload_len = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]);
    Ok((request_id, frame_type, payload_len))
}

// -- Client -------------------------------------------------------------------

/// Persistent multiplexed credential tunnel client.
pub struct CredentialMuxClient {
    writer: Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
    pending: PendingMap,
    next_request_id: AtomicU32,
    reader_handle: Option<JoinHandle<()>>,
}

impl CredentialMuxClient {
    /// Open a credential tunnel connection to the daemon.
    ///
    /// Sends magic byte `0x03`, splits the stream, and spawns a background
    /// reader task that dispatches response frames to waiting callers.
    pub async fn connect(daemon_addr: &str) -> io::Result<Self> {
        let mut stream = TcpStream::connect(daemon_addr).await?;
        stream.write_all(&[MAGIC_CREDENTIAL_PROXY]).await?;
        stream.flush().await?;

        let (reader, writer) = tokio::io::split(stream);
        let writer = Arc::new(Mutex::new(writer));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

        let reader_handle = spawn_reader(reader, pending.clone());

        Ok(Self {
            writer,
            pending,
            next_request_id: AtomicU32::new(1),
            reader_handle: Some(reader_handle),
        })
    }

    /// Proxy an intercepted credential request through the multiplexed tunnel.
    pub async fn proxy_request(
        &self,
        req: hyper::Request<hyper::body::Incoming>,
        host: &str,
        route: &CredentialRoute,
        daemon_token: &str,
        container_name: &str,
        container_nonce: Option<&str>,
    ) -> Result<hyper::Response<BoxBody>, Box<dyn std::error::Error + Send + Sync>> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let trace_id = format!("cred-{request_id:08x}");

        // Register the in-flight request before sending any frames.
        // body_tx starts as None — the reader creates a channel when it
        // receives the RESPONSE_ENVELOPE frame.
        let (envelope_tx, envelope_rx) = oneshot::channel();
        {
            let mut map = self.pending.lock().await;
            map.insert(
                request_id,
                InFlightRequest {
                    envelope_tx: Some(envelope_tx),
                    body_tx: None,
                },
            );
        }

        let result = self
            .send_request_frames(
                request_id,
                &trace_id,
                req,
                host,
                route,
                daemon_token,
                container_name,
                container_nonce,
            )
            .await;

        if let Err(e) = result {
            self.pending.lock().await.remove(&request_id);
            return Err(e);
        }

        // Wait for the reader task to deliver the response.
        match envelope_rx.await {
            Ok(ResponseData::Envelope { envelope, body_rx }) => {
                build_hyper_response(&envelope, body_rx)
            }
            Ok(ResponseData::Error { category, message }) => {
                Ok(build_error_response(&category, &message))
            }
            Err(_) => Ok(build_error_response(
                "connection_lost",
                "Credential tunnel connection lost",
            )),
        }
    }

    /// Send all request frames (handshake, envelope, body chunks, end).
    #[expect(clippy::too_many_arguments)]
    async fn send_request_frames(
        &self,
        request_id: u32,
        trace_id: &str,
        req: hyper::Request<hyper::body::Incoming>,
        host: &str,
        route: &CredentialRoute,
        daemon_token: &str,
        container_name: &str,
        container_nonce: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (parts, body) = req.into_parts();
        let body_bytes = body.collect().await?.to_bytes();

        if body_bytes.len() > MAX_REQUEST_BODY {
            return Err("request body too large".into());
        }

        let handshake = build_handshake(
            host,
            route,
            daemon_token,
            container_name,
            container_nonce,
            trace_id,
        );
        let envelope = build_envelope(&parts);

        let mut writer = self.writer.lock().await;
        send_handshake(&mut *writer, request_id, &handshake).await?;
        send_envelope(&mut *writer, request_id, &envelope).await?;
        send_body_chunks(&mut *writer, request_id, &body_bytes).await?;
        write_frame(&mut *writer, request_id, FRAME_REQUEST_END, &[]).await?;
        drop(writer);

        Ok(())
    }
}

impl Drop for CredentialMuxClient {
    fn drop(&mut self) {
        if let Some(h) = self.reader_handle.take() {
            h.abort();
        }
    }
}

// -- Send helpers (extracted for 100-LOC limit) -------------------------------

fn build_handshake(
    host: &str,
    route: &CredentialRoute,
    daemon_token: &str,
    container_name: &str,
    container_nonce: Option<&str>,
    trace_id: &str,
) -> cella_protocol::CredentialProxyHandshake {
    cella_protocol::CredentialProxyHandshake {
        auth_token: daemon_token.to_string(),
        container_name: container_name.to_string(),
        request_id: trace_id.to_string(),
        domain: host.to_string(),
        provider_id: route.provider_id.clone(),
        container_nonce: container_nonce.map(String::from),
        trace_id: Some(trace_id.to_string()),
    }
}

fn build_envelope(parts: &hyper::http::request::Parts) -> MuxRequestEnvelope {
    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    let uri_str = parts
        .uri
        .path_and_query()
        .map_or("/", |pq| pq.as_str())
        .to_string();

    MuxRequestEnvelope {
        method: parts.method.to_string(),
        uri: uri_str,
        headers,
    }
}

async fn send_handshake<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    request_id: u32,
    handshake: &cella_protocol::CredentialProxyHandshake,
) -> io::Result<()> {
    let payload = serde_json::to_vec(handshake)
        .map_err(|e| io::Error::other(format!("handshake serialization: {e}")))?;
    write_frame(writer, request_id, FRAME_HANDSHAKE, &payload).await
}

async fn send_envelope<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    request_id: u32,
    envelope: &MuxRequestEnvelope,
) -> io::Result<()> {
    let payload = serde_json::to_vec(envelope)
        .map_err(|e| io::Error::other(format!("envelope serialization: {e}")))?;
    write_frame(writer, request_id, FRAME_REQUEST_ENVELOPE, &payload).await
}

async fn send_body_chunks<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    request_id: u32,
    body: &[u8],
) -> io::Result<()> {
    if body.is_empty() {
        return Ok(());
    }
    for chunk in body.chunks(MAX_REQUEST_CHUNK) {
        write_frame(writer, request_id, FRAME_REQUEST_CHUNK, chunk).await?;
    }
    Ok(())
}

// -- Reader task --------------------------------------------------------------

fn spawn_reader(reader: tokio::io::ReadHalf<TcpStream>, pending: PendingMap) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = reader_loop(reader, &pending).await {
            debug!("Credential mux reader ended: {e}");
        }
        // Connection lost — fail all pending requests.
        fail_all_pending(&pending).await;
    })
}

async fn reader_loop(
    mut reader: tokio::io::ReadHalf<TcpStream>,
    pending: &PendingMap,
) -> io::Result<()> {
    loop {
        let (request_id, frame_type, payload_len) = read_frame_header(&mut reader).await?;
        let payload = read_payload(&mut reader, payload_len).await?;
        dispatch_frame(request_id, frame_type, &payload, pending).await;
    }
}

async fn read_payload<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    payload_len: u32,
) -> io::Result<Vec<u8>> {
    let len = payload_len as usize;
    let mut buf = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut buf).await?;
    }
    Ok(buf)
}

async fn dispatch_frame(request_id: u32, frame_type: u8, payload: &[u8], pending: &PendingMap) {
    match frame_type {
        FRAME_RESPONSE_ENVELOPE => handle_response_envelope(request_id, payload, pending).await,
        FRAME_RESPONSE_CHUNK => handle_response_chunk(request_id, payload, pending).await,
        FRAME_RESPONSE_END => handle_response_end(request_id, pending).await,
        FRAME_ERROR => handle_error_frame(request_id, payload, pending).await,
        _ => {
            warn!("Unknown frame type {frame_type:#04x} for request {request_id}");
        }
    }
}

async fn handle_response_envelope(request_id: u32, payload: &[u8], pending: &PendingMap) {
    let envelope: MuxResponseEnvelope = match serde_json::from_slice(payload) {
        Ok(e) => e,
        Err(e) => {
            warn!("Failed to parse response envelope for {request_id}: {e}");
            send_error_to_pending(
                request_id,
                "protocol_violation",
                "bad response envelope",
                pending,
            )
            .await;
            return;
        }
    };

    let mut map = pending.lock().await;
    let Some(in_flight) = map.get_mut(&request_id) else {
        return;
    };

    if let Some(envelope_tx) = in_flight.envelope_tx.take() {
        // Create a channel for body chunk streaming.
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        in_flight.body_tx = Some(tx);
        drop(map);
        let _ = envelope_tx.send(ResponseData::Envelope {
            envelope,
            body_rx: rx,
        });
    }
}

async fn handle_response_chunk(request_id: u32, payload: &[u8], pending: &PendingMap) {
    let tx = {
        let map = pending.lock().await;
        map.get(&request_id).and_then(|f| f.body_tx.clone())
    };
    if let Some(tx) = tx {
        let _ = tx
            .send(Ok(hyper::body::Frame::data(Bytes::copy_from_slice(
                payload,
            ))))
            .await;
    }
}

async fn handle_response_end(request_id: u32, pending: &PendingMap) {
    // Remove the request — dropping body_tx closes the channel, which
    // signals EOF to the StreamBody consumer.
    pending.lock().await.remove(&request_id);
}

async fn handle_error_frame(request_id: u32, payload: &[u8], pending: &PendingMap) {
    let (category, message) = match serde_json::from_slice::<ErrorEnvelope>(payload) {
        Ok(e) => (e.category, e.message),
        Err(_) => (
            "protocol_violation".to_string(),
            "unparseable error frame".to_string(),
        ),
    };
    send_error_to_pending(request_id, &category, &message, pending).await;
}

async fn send_error_to_pending(
    request_id: u32,
    category: &str,
    message: &str,
    pending: &PendingMap,
) {
    if let Some(mut in_flight) = pending.lock().await.remove(&request_id)
        && let Some(tx) = in_flight.envelope_tx.take()
    {
        let _ = tx.send(ResponseData::Error {
            category: category.to_string(),
            message: message.to_string(),
        });
    }
}

async fn fail_all_pending(pending: &PendingMap) {
    let mut map = pending.lock().await;
    for (_, mut in_flight) in map.drain() {
        if let Some(tx) = in_flight.envelope_tx.take() {
            let _ = tx.send(ResponseData::Error {
                category: "connection_lost".to_string(),
                message: "credential tunnel connection lost".to_string(),
            });
        }
        // Drop body_tx to close body streams.
    }
}

// -- Response building --------------------------------------------------------

fn build_hyper_response(
    envelope: &MuxResponseEnvelope,
    body_rx: tokio::sync::mpsc::Receiver<BodyFrame>,
) -> Result<hyper::Response<BoxBody>, Box<dyn std::error::Error + Send + Sync>> {
    let body =
        http_body_util::StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(body_rx));

    let mut builder = hyper::Response::builder().status(envelope.status);
    for (key, value) in &envelope.headers {
        builder = builder.header(key.as_str(), value.as_str());
    }

    Ok(builder.body(body.boxed())?)
}

fn build_error_response(category: &str, message: &str) -> hyper::Response<BoxBody> {
    let status = match category {
        "nonce_invalid" => 401,
        "token_invalid"
        | "provider_mismatch"
        | "domain_unregistered"
        | "credential_unavailable" => 403,
        "too_many_requests" => 503,
        _ => 502,
    };

    hyper::Response::builder()
        .status(status)
        .header("x-cella-error", category)
        .body(
            Full::new(Bytes::from(message.to_string()))
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
                .boxed(),
        )
        .expect("building error response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_header_roundtrip() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut buf = Vec::new();
            write_frame(&mut buf, 42, FRAME_HANDSHAKE, b"hello")
                .await
                .unwrap();

            assert_eq!(buf.len(), FRAME_HEADER_SIZE + 5);

            let mut cursor = io::Cursor::new(&buf);
            let (id, ft, plen) = read_frame_header(&mut cursor).await.unwrap();
            assert_eq!(id, 42);
            assert_eq!(ft, FRAME_HANDSHAKE);
            assert_eq!(plen, 5);

            let payload = read_payload(&mut cursor, plen).await.unwrap();
            assert_eq!(payload, b"hello");
        });
    }

    #[test]
    fn frame_empty_payload_roundtrip() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut buf = Vec::new();
            write_frame(&mut buf, 1, FRAME_REQUEST_END, &[])
                .await
                .unwrap();

            assert_eq!(buf.len(), FRAME_HEADER_SIZE);

            let mut cursor = io::Cursor::new(&buf);
            let (id, ft, plen) = read_frame_header(&mut cursor).await.unwrap();
            assert_eq!(id, 1);
            assert_eq!(ft, FRAME_REQUEST_END);
            assert_eq!(plen, 0);

            let payload = read_payload(&mut cursor, plen).await.unwrap();
            assert!(payload.is_empty());
        });
    }

    #[test]
    fn request_id_monotonic() {
        let counter = AtomicU32::new(1);
        let ids: Vec<u32> = (0..100)
            .map(|_| counter.fetch_add(1, Ordering::Relaxed))
            .collect();
        for window in ids.windows(2) {
            assert_eq!(window[1], window[0] + 1);
        }
    }

    #[test]
    fn error_envelope_parsing() {
        let json = r#"{"category":"token_invalid","message":"Phantom token not found"}"#;
        let env: ErrorEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.category, "token_invalid");
        assert_eq!(env.message, "Phantom token not found");
    }

    #[test]
    fn error_response_status_mapping() {
        let resp = build_error_response("nonce_invalid", "bad nonce");
        assert_eq!(resp.status(), 401);

        let resp = build_error_response("token_invalid", "bad token");
        assert_eq!(resp.status(), 403);

        let resp = build_error_response("provider_mismatch", "mismatch");
        assert_eq!(resp.status(), 403);

        let resp = build_error_response("too_many_requests", "limit");
        assert_eq!(resp.status(), 503);

        let resp = build_error_response("upstream_error", "dns failed");
        assert_eq!(resp.status(), 502);

        let resp = build_error_response("connection_lost", "gone");
        assert_eq!(resp.status(), 502);
    }

    #[test]
    fn error_response_has_cella_error_header() {
        let resp = build_error_response("token_invalid", "bad");
        assert_eq!(
            resp.headers()
                .get("x-cella-error")
                .unwrap()
                .to_str()
                .unwrap(),
            "token_invalid"
        );
    }

    #[test]
    fn request_envelope_serialization() {
        let env = MuxRequestEnvelope {
            method: "POST".to_string(),
            uri: "/v1/messages".to_string(),
            headers: vec![("x-api-key".to_string(), "pt-abc".to_string())],
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"method\":\"POST\""));
        // No body_len field in the new envelope.
        assert!(!json.contains("body_len"));
    }

    #[test]
    fn response_envelope_deserialization() {
        let json = r#"{"status":200,"headers":[["content-type","application/json"]]}"#;
        let env: MuxResponseEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.status, 200);
        assert_eq!(env.headers.len(), 1);
    }

    #[test]
    fn multiple_frames_roundtrip() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut buf = Vec::new();
            write_frame(&mut buf, 1, FRAME_HANDSHAKE, b"hs")
                .await
                .unwrap();
            write_frame(&mut buf, 1, FRAME_REQUEST_ENVELOPE, b"env")
                .await
                .unwrap();
            write_frame(&mut buf, 1, FRAME_REQUEST_END, &[])
                .await
                .unwrap();

            let mut cursor = io::Cursor::new(&buf);

            let (id, ft, plen) = read_frame_header(&mut cursor).await.unwrap();
            assert_eq!((id, ft), (1, FRAME_HANDSHAKE));
            let p = read_payload(&mut cursor, plen).await.unwrap();
            assert_eq!(p, b"hs");

            let (id, ft, plen) = read_frame_header(&mut cursor).await.unwrap();
            assert_eq!((id, ft), (1, FRAME_REQUEST_ENVELOPE));
            let p = read_payload(&mut cursor, plen).await.unwrap();
            assert_eq!(p, b"env");

            let (id, ft, plen) = read_frame_header(&mut cursor).await.unwrap();
            assert_eq!((id, ft), (1, FRAME_REQUEST_END));
            assert_eq!(plen, 0);
        });
    }

    #[test]
    fn frame_type_constants_match_spec() {
        assert_eq!(FRAME_HANDSHAKE, 0x01);
        assert_eq!(FRAME_REQUEST_ENVELOPE, 0x02);
        assert_eq!(FRAME_REQUEST_CHUNK, 0x03);
        assert_eq!(FRAME_REQUEST_END, 0x04);
        assert_eq!(FRAME_RESPONSE_ENVELOPE, 0x05);
        assert_eq!(FRAME_RESPONSE_CHUNK, 0x06);
        assert_eq!(FRAME_RESPONSE_END, 0x07);
        assert_eq!(FRAME_ERROR, 0x08);
    }
}
