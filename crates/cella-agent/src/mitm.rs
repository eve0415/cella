use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::task::{Context, Poll};

use http_body_util::BodyExt;
use hyper::body::{Body, Incoming};
use hyper::client::conn::TrySendError;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rcgen::{CertificateParams, DnType, DnValue, IsCa, Issuer, KeyPair, SanType};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, warn};

use crate::proxy_config::AgentProxyConfig;
use crate::proxy_http::{
    BoxBody, GatewayError, Sent, await_pooled, full, has_invalid_token_lines, is_websocket_upgrade,
    offer_only_websocket, relay_upgrade, strip_hop_by_hop, text_response,
};
use crate::respond::{self, Channel, Decision};

static NATIVE_ROOTS: OnceLock<Arc<rustls::RootCertStore>> = OnceLock::new();

/// Extra upstream connections one intercepted client connection may open,
/// beyond the first, while every open one is busy with an earlier response.
/// Past these, a request waits for an open one to come free.
const MAX_EXTRA_UPSTREAMS: usize = 8;

/// The upstream connections of one intercepted client connection, opened
/// lazily.
struct Upstream<B = Incoming> {
    /// Open connections: one for an HTTP/2 upstream, and for an HTTP/1.1
    /// upstream another whenever every open one is still busy.
    senders: Mutex<Senders<B>>,
    /// Held while opening a connection when none is open, so requests that
    /// arrive together share the first connection rather than each opening
    /// their own.
    first_connect: tokio::sync::Mutex<()>,
    /// Held by the one request watching for a busy connection to come free,
    /// so requests that find every connection busy and none to add wait in
    /// turn instead of each polling the connections.
    waiting: tokio::sync::Mutex<()>,
    /// One permit per open connection, released when the connection ends.
    permits: Arc<Semaphore>,
    /// Notified whenever a connection joins or leaves `senders`, so the
    /// waiting request watches the connections open now rather than those
    /// open when it started waiting.
    changed: Notify,
}

/// What a request finds once it has waited out every connection being busy.
enum Room {
    /// An open connection came free, or closed.
    Freed,
    /// Another connection may be opened.
    New(OwnedSemaphorePermit),
}

impl<B> Upstream<B> {
    fn new() -> Self {
        Self {
            senders: Mutex::new(Senders(Vec::new())),
            first_connect: tokio::sync::Mutex::new(()),
            waiting: tokio::sync::Mutex::new(()),
            permits: Arc::new(Semaphore::new(1 + MAX_EXTRA_UPSTREAMS)),
            changed: Notify::new(),
        }
    }

    /// Reserve room for one more open connection, waiting while the most
    /// are open.
    async fn reserve(&self) -> Result<OwnedSemaphorePermit, String> {
        Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|e| format!("upstream connection limit: {e}"))
    }

    /// Wait until a busy connection comes free or another may be opened,
    /// preferring a free one.
    ///
    /// Each pass registers for a change to the open connections before
    /// looking at them, so a connection added after the look wakes it, and
    /// one added before, even while it waited for its turn, is in the look.
    async fn room(&self) -> Result<Room, String> {
        if let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() {
            return Ok(Room::New(permit));
        }
        let turn = self.waiting.lock().await;
        let room = loop {
            let mut changed = std::pin::pin!(self.changed.notified());
            changed.as_mut().enable();
            tokio::select! {
                biased;
                () = std::future::poll_fn(|cx| self.poll_freed(cx)) => break Room::Freed,
                () = changed => {}
                permit = self.reserve() => break Room::New(permit?),
            }
        };
        drop(turn);
        Ok(room)
    }

    /// The open connections, with those that closed dropped.
    fn open_senders(&self) -> MutexGuard<'_, Senders<B>> {
        let mut senders = self.senders();
        if senders.prune_closed() {
            self.changed.notify_waiters();
        }
        senders
    }

    /// Ready once an open connection can take a request or has closed.
    fn poll_freed(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self
            .senders()
            .0
            .iter_mut()
            .any(|sender| sender.poll_ready(cx).is_ready())
        {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    /// The open connections, locked only for synchronous bookkeeping.
    fn senders(&self) -> MutexGuard<'_, Senders<B>> {
        self.senders.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<B: Body + Send + 'static> Upstream<B> {
    /// Start `req` on the first open connection that can take it now.
    fn checkout(&self, req: Request<B>) -> Checkout<B> {
        self.open_senders().checkout(req)
    }

    /// Keep a newly opened connection and start `req` on it.
    fn add(&self, mut sender: UpstreamSender<B>, req: Request<B>) -> SendFuture<B> {
        let sending = sender.try_send(req);
        self.senders().0.push(sender);
        self.changed.notify_waiters();
        sending
    }
}

/// A request started on an upstream connection.
type SendFuture<B> =
    Pin<Box<dyn Future<Output = Result<Response<Incoming>, TrySendError<Request<B>>>> + Send>>;

/// Where a request can go among the open upstream connections.
enum Checkout<B> {
    /// Started on an open connection that was free.
    Sent(SendFuture<B>),
    /// Every open connection is busy with an earlier response.
    Busy(Request<B>),
    /// No connection is open.
    Vacant(Request<B>),
}

struct Senders<B>(Vec<UpstreamSender<B>>);

impl<B> Senders<B> {
    /// Drop the closed connections, returning whether there were any.
    fn prune_closed(&mut self) -> bool {
        let open = self.0.len();
        self.0.retain(|sender| !sender.is_closed());
        self.0.len() != open
    }
}

impl<B: Body + Send + 'static> Senders<B> {
    /// Start `req` on the first connection that can take it now. Closed
    /// connections must already be pruned.
    fn checkout(&mut self, req: Request<B>) -> Checkout<B> {
        if self.0.is_empty() {
            return Checkout::Vacant(req);
        }
        match self.0.iter_mut().find(|sender| sender.is_ready()) {
            Some(sender) => Checkout::Sent(sender.try_send(req)),
            None => Checkout::Busy(req),
        }
    }
}

enum UpstreamSender<B = Incoming> {
    Http1(hyper::client::conn::http1::SendRequest<B>),
    Http2(hyper::client::conn::http2::SendRequest<B>),
}

impl<B> UpstreamSender<B> {
    fn is_closed(&self) -> bool {
        match self {
            Self::Http1(s) => s.is_closed(),
            Self::Http2(s) => s.is_closed(),
        }
    }

    /// Whether a request can go out now without waiting for an earlier
    /// response on this connection to finish.
    fn is_ready(&self) -> bool {
        match self {
            Self::Http1(s) => s.is_ready(),
            Self::Http2(s) => s.is_ready(),
        }
    }

    /// Ready once a request can go out without waiting, or the connection
    /// closed. Only one task may wait on a connection at a time: a second
    /// one takes over the wake-up from the first.
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<hyper::Result<()>> {
        match self {
            Self::Http1(s) => s.poll_ready(cx),
            Self::Http2(s) => s.poll_ready(cx),
        }
    }
}

impl<B: Body + Send + 'static> UpstreamSender<B> {
    /// Start `req` without waiting for the connection to become ready. The
    /// returned future does not borrow the sender.
    fn try_send(&mut self, req: Request<B>) -> SendFuture<B> {
        match self {
            Self::Http1(s) => Box::pin(s.try_send_request(req)),
            Self::Http2(s) => Box::pin(s.try_send_request(req)),
        }
    }
}

pub async fn intercept_tls<S>(client: S, host: &str, port: u16, config: Arc<AgentProxyConfig>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let Some(client_tls) = accept_client_tls(client, host, &config).await else {
        return;
    };

    // Connected on the first forwarded request, so a connection answered
    // entirely by `respond` rules (or the credential route) never opens one.
    let upstream = Arc::new(Upstream::new());
    let host_owned: Arc<str> = host.into();
    let client_io = TokioIo::new(client_tls);

    let svc = service_fn(move |req: Request<Incoming>| {
        let upstream = upstream.clone();
        let host = host_owned.clone();
        let config = config.clone();
        async move { handle_request(req, &host, port, &config, &upstream).await }
    });

    let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    if let Err(e) = builder.serve_connection_with_upgrades(client_io, svc).await {
        debug!("MITM server for {host} ended: {e}");
    }
}

async fn accept_client_tls<S>(
    client: S,
    host: &str,
    config: &AgentProxyConfig,
) -> Option<tokio_rustls::server::TlsStream<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let tls_config = match generate_server_config(host, config) {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!("Failed to generate MITM cert for {host}: {e}");
            config.log_error(host, &format!("MITM cert generation failed: {e}"));
            return None;
        }
    };
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));
    match acceptor.accept(client).await {
        Ok(s) => Some(s),
        Err(e) => {
            warn!("TLS handshake failed for {host}: {e}");
            config.log_error(host, &format!("TLS handshake failed: {e}"));
            None
        }
    }
}

/// Open an upstream connection whose task holds `permit` until it ends.
async fn connect_upstream_tls(
    host: &str,
    port: u16,
    config: &AgentProxyConfig,
    permit: OwnedSemaphorePermit,
) -> Result<UpstreamSender, String> {
    let upstream_tcp = super::forward_proxy::connect_upstream_for_mitm(host, port, config)
        .await
        .map_err(|e| format!("upstream connect failed: {e}"))?;

    let extra = config.ca_cert_der.as_ref().map(std::slice::from_ref);
    let connector = tokio_rustls::TlsConnector::from(Arc::new(upstream_tls_config(extra)));
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("invalid server name: {e}"))?;
    let upstream_tls = connector
        .connect(server_name, upstream_tcp)
        .await
        .map_err(|e| format!("upstream TLS failed: {e}"))?;

    let is_h2 = upstream_tls
        .get_ref()
        .1
        .alpn_protocol()
        .is_some_and(|p| p == b"h2");
    let upstream_io = TokioIo::new(upstream_tls);

    if is_h2 {
        let (sender, conn) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), upstream_io)
                .await
                .map_err(|e| format!("upstream HTTP/2 handshake failed: {e}"))?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                debug!("Upstream h2 connection ended: {e}");
            }
            drop(permit);
        });
        Ok(UpstreamSender::Http2(sender))
    } else {
        let (sender, conn) = hyper::client::conn::http1::handshake(upstream_io)
            .await
            .map_err(|e| format!("upstream HTTP/1.1 handshake failed: {e}"))?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                debug!("Upstream h1 connection ended: {e}");
            }
            drop(permit);
        });
        Ok(UpstreamSender::Http1(sender))
    }
}

async fn connect_h1_upstream(
    host: &str,
    port: u16,
    config: &AgentProxyConfig,
) -> Result<hyper::client::conn::http1::SendRequest<Incoming>, String> {
    let upstream_tcp = super::forward_proxy::connect_upstream_for_mitm(host, port, config)
        .await
        .map_err(|e| format!("upstream connect: {e}"))?;

    let extra = config.ca_cert_der.as_ref().map(std::slice::from_ref);
    let mut tls_config = upstream_tls_config(extra);
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("invalid server name: {e}"))?;
    let upstream_tls = connector
        .connect(server_name, upstream_tcp)
        .await
        .map_err(|e| format!("upstream TLS: {e}"))?;

    let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(upstream_tls))
        .await
        .map_err(|e| format!("upstream h1 handshake: {e}"))?;
    tokio::spawn(async move {
        if let Err(e) = conn.with_upgrades().await {
            debug!("Upgrade upstream connection ended: {e}");
        }
    });
    Ok(sender)
}

async fn handle_upgrade_request(
    mut req: Request<Incoming>,
    host: &str,
    port: u16,
    config: &AgentProxyConfig,
) -> Result<Response<BoxBody>, GatewayError> {
    let client_upgrade = hyper::upgrade::on(&mut req);
    let mut req = prepare_forwarded_request(req, host, true);
    offer_only_websocket(req.headers_mut());

    let mut sender = match connect_h1_upstream(host, port, config).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Upgrade upstream connection to {host} failed: {e}");
            return Err(GatewayError::BadGateway);
        }
    };

    let resp = match sender.send_request(req).await {
        Ok(r) => r,
        Err(e) => {
            warn!("Upgrade request to {host} failed: {e}");
            return Err(GatewayError::BadGateway);
        }
    };

    relay_upgrade(client_upgrade, resp, host)
}

async fn handle_request(
    req: Request<Incoming>,
    host: &str,
    port: u16,
    config: &AgentProxyConfig,
    upstream: &Upstream,
) -> Result<Response<BoxBody>, std::convert::Infallible> {
    // A field line `header_tokens` would skip could hide an `Upgrade` token
    // or a header listed for stripping, so it is refused before any routing.
    if has_invalid_token_lines(req.headers()) {
        return Ok(text_response(StatusCode::BAD_REQUEST, "Bad Request"));
    }
    if let Some(route) = config.credential_route_for_domain(host)
        && let (Some(addr), Some(token), Some(name)) = (
            &config.daemon_addr,
            &config.daemon_token,
            &config.container_name,
        )
    {
        return Ok(route_credential_request(req, host, route, addr, token, name, config).await);
    }

    let (req, response_headers) = match respond::decide(req, host, config, Channel::Mitm).await {
        Decision::Done(resp) => return Ok(resp),
        Decision::Forward(req, response_headers) => (req, response_headers),
    };

    // Only WebSocket upgrades are spliced. Any other upgrade (such as h2c) is
    // forwarded as an ordinary request with its upgrade headers stripped, so
    // later requests on the connection still go through the rules. HTTP/2
    // clients use Extended CONNECT (RFC 8441) instead of Upgrade headers, so
    // this only triggers for HTTP/1.1 clients.
    let forwarded = if is_websocket_upgrade(&req) {
        handle_upgrade_request(req, host, port, config).await
    } else {
        forward_request(req, host, port, config, upstream).await
    };
    Ok(respond::forwarded_response(
        forwarded,
        response_headers,
        host,
        config,
    ))
}

/// Forward an allowed request on a free upstream connection of this client
/// connection, opening one when none is free.
///
/// No lock is held while the request is in flight, so a long response never
/// holds up the client's other requests.
async fn forward_request(
    req: Request<Incoming>,
    host: &str,
    port: u16,
    config: &AgentProxyConfig,
    upstream: &Upstream,
) -> Result<Response<BoxBody>, GatewayError> {
    let req = prepare_forwarded_request(req, host, false);
    let connect = |permit| Box::pin(connect_upstream_tls(host, port, config, permit));
    match send_pooled(req, upstream, connect, host).await {
        Ok(mut resp) => {
            strip_hop_by_hop(resp.headers_mut(), false);
            Ok(resp.map(|body| body.map_err(Into::into).boxed()))
        }
        Err(e) => {
            warn!("Upstream request to {host}:{port} failed: {e}");
            config.log_error(host, &e);
            Err(GatewayError::BadGateway)
        }
    }
}

/// Send `req` on a free open connection of `upstream`, or on one `connect`
/// opens when none is free. With the most connections open and all busy, it
/// waits for one to come free; the client's own timeouts bound the wait.
///
/// A request an open connection hands back unsent, because it closed first,
/// goes through the same choice again; one that went out is never resent.
/// When no connection is open, this first waits for any request already
/// opening one, and uses that connection instead if it is free.
async fn send_pooled<B, C, CF>(
    mut req: Request<B>,
    upstream: &Upstream<B>,
    connect: C,
    host: &str,
) -> Result<Response<Incoming>, String>
where
    B: Body + Send + 'static,
    C: Fn(OwnedSemaphorePermit) -> CF + Sync,
    CF: Future<Output = Result<UpstreamSender<B>, String>> + Send,
{
    loop {
        let checkout = upstream.checkout(req);
        req = match checkout {
            Checkout::Sent(sending) => match await_pooled(sending, host).await? {
                Sent::Answered(resp) => return Ok(resp),
                Sent::Returned(req) => *req,
            },
            Checkout::Busy(req) => {
                let room = upstream.room().await?;
                match room {
                    Room::Freed => req,
                    Room::New(permit) => {
                        return send_on_new(req, upstream, connect(permit), None).await;
                    }
                }
            }
            Checkout::Vacant(req) => {
                let gate = upstream.first_connect.lock().await;
                if upstream.open_senders().0.is_empty() {
                    let connecting = connect(upstream.reserve().await?);
                    return send_on_new(req, upstream, connecting, Some(gate)).await;
                }
                req
            }
        };
    }
}

/// Send `req` on the connection `connecting` opens, keeping the connection
/// for later requests. `gate` is released once the connection is kept.
async fn send_on_new<B: Body + Send + 'static>(
    req: Request<B>,
    upstream: &Upstream<B>,
    connecting: impl Future<Output = Result<UpstreamSender<B>, String>>,
    gate: Option<tokio::sync::MutexGuard<'_, ()>>,
) -> Result<Response<Incoming>, String> {
    let sender = connecting.await?;
    let sending = upstream.add(sender, req);
    drop(gate);
    sending.await.map_err(|e| e.into_error().to_string())
}

fn prepare_forwarded_request(
    req: Request<Incoming>,
    host: &str,
    preserve_upgrade: bool,
) -> Request<Incoming> {
    let (mut parts, body) = req.into_parts();

    if parts.uri.authority().is_none() {
        let pq = parts
            .uri
            .path_and_query()
            .map_or("/", hyper::http::uri::PathAndQuery::as_str);
        if let Ok(uri) = hyper::Uri::builder()
            .scheme("https")
            .authority(host)
            .path_and_query(pq)
            .build()
        {
            parts.uri = uri;
        }
    }

    strip_hop_by_hop(&mut parts.headers, preserve_upgrade);

    Request::from_parts(parts, body)
}

/// Route a credential request through the mux client, falling back to the
/// legacy per-request tunnel on initial connection failure.
async fn route_credential_request(
    req: Request<Incoming>,
    host: &str,
    route: &crate::proxy_config::CredentialRoute,
    daemon_addr: &str,
    daemon_token: &str,
    container_name: &str,
    config: &AgentProxyConfig,
) -> Response<BoxBody> {
    let mux = get_or_connect_mux(config, daemon_addr).await;

    if let Some(client) = mux {
        match client
            .proxy_request(
                req,
                host,
                route,
                daemon_token,
                container_name,
                config.container_nonce.as_deref(),
            )
            .await
        {
            Ok(resp) => return resp,
            Err(e) => {
                warn!("Credential mux request failed: {e}");
            }
        }
    } else {
        // Mux unavailable — fall back to legacy per-request tunnel.
        return super::credential_tunnel::tunnel_request(
            req,
            host,
            route,
            daemon_addr,
            daemon_token,
            container_name,
            config.container_nonce.as_deref(),
        )
        .await;
    }

    Response::builder()
        .status(502)
        .body(full("Credential proxy unavailable"))
        .expect("building 502 response")
}

/// Get or lazily initialize the mux client.
async fn get_or_connect_mux(
    config: &AgentProxyConfig,
    daemon_addr: &str,
) -> Option<Arc<super::credential_mux::CredentialMuxClient>> {
    let mut guard = config.credential_mux.lock().await;
    if let Some(ref client) = *guard {
        return Some(client.clone());
    }

    match super::credential_mux::CredentialMuxClient::connect(daemon_addr).await {
        Ok(client) => {
            let client = Arc::new(client);
            *guard = Some(client.clone());
            drop(guard);
            debug!("Credential mux connected to {daemon_addr}");
            Some(client)
        }
        Err(e) => {
            debug!("Credential mux connect failed, using legacy tunnel: {e}");
            None
        }
    }
}

fn generate_server_config(domain: &str, config: &AgentProxyConfig) -> Result<ServerConfig, String> {
    let ca = load_ca_materials(config)?;
    let ca_issuer = Issuer::from_params(&ca.params, &ca.key_pair);

    let domain_key = KeyPair::generate().map_err(|e| format!("key generation: {e}"))?;

    let mut params = CertificateParams::default();
    params.is_ca = IsCa::NoCa;
    params
        .distinguished_name
        .push(DnType::CommonName, DnValue::Utf8String(domain.to_string()));
    params.subject_alt_names.push(SanType::DnsName(
        domain
            .to_string()
            .try_into()
            .map_err(|e| format!("invalid domain for SAN: {e}"))?,
    ));

    let domain_cert = params
        .signed_by(&domain_key, &ca_issuer)
        .map_err(|e| format!("cert signing: {e}"))?;

    let cert_der = CertificateDer::from(domain_cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(domain_key.serialize_der()));

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .map_err(|e| format!("server config: {e}"))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

struct CaIssuerMaterials {
    params: CertificateParams,
    key_pair: KeyPair,
}

fn load_ca_materials(config: &AgentProxyConfig) -> Result<CaIssuerMaterials, String> {
    let ca_key_pem = config
        .ca_key_pem
        .as_deref()
        .ok_or("no CA key available for MITM")?;

    let ca_key_pair = KeyPair::from_pem(ca_key_pem).map_err(|e| format!("parse CA key: {e}"))?;

    Ok(CaIssuerMaterials {
        params: cella_network::ca::ca_certificate_params(),
        key_pair: ca_key_pair,
    })
}

fn cached_native_roots() -> Arc<rustls::RootCertStore> {
    NATIVE_ROOTS
        .get_or_init(|| {
            let mut root_store = rustls::RootCertStore::empty();
            let native = rustls_native_certs::load_native_certs();
            for cert in native.certs {
                let _ = root_store.add(cert);
            }
            Arc::new(root_store)
        })
        .clone()
}

pub fn upstream_tls_config(
    additional_roots: Option<&[CertificateDer<'_>]>,
) -> rustls::ClientConfig {
    let mut root_store = (*cached_native_roots()).clone();
    if let Some(roots) = additional_roots {
        for cert in roots {
            let _ = root_store.add(cert.clone());
        }
    }

    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy_http::test_upstreams::{
        Body as TestBody, LIVE_BODY, get, live_h1, live_h2, unspawned_h1, unspawned_h2,
    };

    fn install_crypto_provider() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    /// Opens a working HTTP/2 connection in place of the real upstream.
    async fn connect_live(
        permit: OwnedSemaphorePermit,
    ) -> Result<UpstreamSender<TestBody>, String> {
        drop(permit);
        Ok(UpstreamSender::Http2(live_h2().await))
    }

    /// Poll `fut` once, which must leave it pending.
    async fn poll_once<F: Future>(fut: Pin<&mut F>) {
        let mut fut = Some(fut);
        std::future::poll_fn(|cx| {
            let pending = fut.take().unwrap().poll(cx).is_pending();
            assert!(pending, "finished on its first poll");
            Poll::Ready(())
        })
        .await;
    }

    /// Two pooled connections, each ready when the request is handed to it and
    /// ended before it goes out.
    async fn pooled_then_ended() -> Vec<(UpstreamSender<TestBody>, Pin<Box<dyn Send>>)> {
        let (h1, h1_conn) = unspawned_h1().await;
        let (h2, h2_conn) = unspawned_h2().await;
        vec![
            (UpstreamSender::Http1(h1), h1_conn as Pin<Box<dyn Send>>),
            (UpstreamSender::Http2(h2), h2_conn as Pin<Box<dyn Send>>),
        ]
    }

    #[tokio::test]
    async fn resends_when_pooled_connection_closes_before_the_request_goes_out() {
        for (sender, conn) in pooled_then_ended().await {
            let upstream = Upstream::new();
            upstream.senders().0.push(sender);
            let mut sending =
                std::pin::pin!(send_pooled(get(), &upstream, connect_live, "upstream.test"));
            poll_once(sending.as_mut()).await;
            drop(conn);
            let resp = sending.await.expect("request was not resent");
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(body, LIVE_BODY);
        }
    }

    /// A request waiting for another to open the first connection lands on
    /// that connection, which ends before the request goes out.
    #[tokio::test]
    async fn resends_when_connection_opened_meanwhile_closes_before_the_request_goes_out() {
        for (sender, conn) in pooled_then_ended().await {
            let upstream = Upstream::new();
            let opening = upstream.first_connect.lock().await;
            let mut sending =
                std::pin::pin!(send_pooled(get(), &upstream, connect_live, "upstream.test"));
            poll_once(sending.as_mut()).await;
            upstream.senders().0.push(sender);
            drop(opening);
            poll_once(sending.as_mut()).await;
            drop(conn);
            let resp = sending.await.expect("request was not resent");
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(body, LIVE_BODY);
        }
    }

    /// With the most connections open and all busy, a waiting request takes a
    /// connection another request opened while it waited, once that one
    /// comes free, without any connection closing or permit coming back.
    #[tokio::test]
    async fn waiting_request_takes_a_connection_opened_while_it_waited() {
        let upstream = Arc::new(Upstream::new());
        let all_permits = u32::try_from(1 + MAX_EXTRA_UPSTREAMS).unwrap();
        let held = Arc::clone(&upstream.permits)
            .try_acquire_many_owned(all_permits)
            .unwrap();
        // Busy with a request it never answers: its connection never runs.
        let (mut busy, busy_conn) = unspawned_h1().await;
        let busy_sending = busy.try_send_request(get());
        assert!(!busy.is_ready() && !busy.is_closed());
        upstream.senders().0.push(UpstreamSender::Http1(busy));

        let waiter = tokio::spawn({
            let upstream = Arc::clone(&upstream);
            async move {
                let refuse = |_| async { Err("opened another connection".to_string()) };
                send_pooled(get(), &upstream, refuse, "upstream.test").await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while upstream.waiting.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("request never started waiting");

        let opened = UpstreamSender::Http1(live_h1().await);
        let resp = send_on_new(get(), &upstream, async { Ok(opened) }, None)
            .await
            .unwrap();
        resp.into_body().collect().await.unwrap();

        let resp = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("waiting request was never woken")
            .unwrap()
            .expect("waiting request failed");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, LIVE_BODY);
        drop((held, busy_sending, busy_conn));
    }

    /// Installing the provider must actually succeed the first time, not just
    /// be swallowed by the `let _ =` the production call sites use. A provider
    /// that fails to install surfaces as a panic on the first handshake, from
    /// inside a spawned task whose stderr the entrypoint discards.
    #[test]
    fn crypto_provider_installs() {
        let installed = rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .is_ok();
        // Another test in this binary may have won the race; either way a
        // provider must be present afterwards.
        assert!(
            installed || rustls::crypto::CryptoProvider::get_default().is_some(),
            "no default crypto provider after install_default"
        );
    }

    /// End-to-end TLS over loopback using the MITM's own server config and an
    /// rcgen-generated CA. The unit tests above only build configs; nothing
    /// else in this crate completes a handshake without Docker, so without
    /// this a broken crypto provider would reach CI unnoticed.
    #[tokio::test]
    async fn loopback_handshake_succeeds_with_installed_provider() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        install_crypto_provider();

        let ca_key = KeyPair::generate().unwrap();
        let ca_params = cella_network::ca::ca_certificate_params();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let json = serde_json::json!({
            "listen_port": 0,
            "mode": "denylist",
            "rules": [],
            "ca_cert_pem": ca_cert.pem(),
            "ca_key_pem": ca_key.serialize_pem(),
        })
        .to_string();
        let config = AgentProxyConfig::from_json(&json).unwrap();
        let server_config = generate_server_config("example.com", &config).unwrap();

        // Trust exactly the CA that signed the leaf the server will present.
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(ca_cert.der().to_vec()))
            .unwrap();
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let acceptor = TlsAcceptor::from(Arc::new(server_config));
            let mut tls = acceptor.accept(stream).await.expect("server handshake");
            tls.write_all(b"pong").await.unwrap();
            tls.shutdown().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
        let domain = rustls::pki_types::ServerName::try_from("example.com").unwrap();
        let mut tls = connector
            .connect(domain, stream)
            .await
            .expect("client handshake");

        let mut buf = Vec::new();
        tls.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"pong");
        server.await.unwrap();
    }

    #[test]
    fn server_config_offers_h2_and_http11_alpn() {
        install_crypto_provider();
        let ca_key = KeyPair::generate().unwrap();
        let ca_params = cella_network::ca::ca_certificate_params();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let json = serde_json::json!({
            "listen_port": 0,
            "mode": "denylist",
            "rules": [],
            "ca_cert_pem": ca_cert.pem(),
            "ca_key_pem": ca_key.serialize_pem(),
        })
        .to_string();
        let config = AgentProxyConfig::from_json(&json).unwrap();
        let server_config = generate_server_config("example.com", &config).unwrap();
        assert_eq!(
            server_config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[test]
    fn upstream_config_offers_h2_and_http11_alpn() {
        install_crypto_provider();
        let config = upstream_tls_config(None);
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[test]
    fn upstream_tls_config_with_custom_roots() {
        install_crypto_provider();
        let key = KeyPair::generate().unwrap();
        let params = cella_network::ca::ca_certificate_params();
        let cert = params.self_signed(&key).unwrap();
        let der = CertificateDer::from(cert.der().to_vec());

        let config = upstream_tls_config(Some(&[der]));
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }
}
