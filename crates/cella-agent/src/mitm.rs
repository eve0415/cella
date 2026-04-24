use std::sync::{Arc, OnceLock};

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rcgen::{CertificateParams, DnType, DnValue, IsCa, Issuer, KeyPair, SanType};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, warn};

use crate::proxy_config::AgentProxyConfig;

type BoxBody =
    http_body_util::combinators::BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

static NATIVE_ROOTS: OnceLock<Arc<rustls::RootCertStore>> = OnceLock::new();

enum UpstreamSender {
    Http1(hyper::client::conn::http1::SendRequest<Incoming>),
    Http2(hyper::client::conn::http2::SendRequest<Incoming>),
}

impl UpstreamSender {
    async fn send_request(&mut self, req: Request<Incoming>) -> hyper::Result<Response<Incoming>> {
        match self {
            Self::Http1(s) => s.send_request(req).await,
            Self::Http2(s) => s.send_request(req).await,
        }
    }
}

pub async fn intercept_tls(
    client: TcpStream,
    host: &str,
    port: u16,
    config: Arc<AgentProxyConfig>,
) {
    let Some(client_tls) = accept_client_tls(client, host, &config).await else {
        return;
    };
    let Some(sender) = connect_upstream_tls(host, port, &config).await else {
        return;
    };

    let sender = Arc::new(Mutex::new(sender));
    let host_owned: Arc<str> = host.into();
    let client_io = TokioIo::new(client_tls);

    let svc = service_fn(move |req: Request<Incoming>| {
        let sender = sender.clone();
        let host = host_owned.clone();
        let config = config.clone();
        async move { handle_request(req, &host, port, &config, &sender).await }
    });

    let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    if let Err(e) = builder.serve_connection_with_upgrades(client_io, svc).await {
        debug!("MITM server for {host} ended: {e}");
    }
}

async fn accept_client_tls(
    client: TcpStream,
    host: &str,
    config: &AgentProxyConfig,
) -> Option<tokio_rustls::server::TlsStream<TcpStream>> {
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

async fn connect_upstream_tls(
    host: &str,
    port: u16,
    config: &AgentProxyConfig,
) -> Option<UpstreamSender> {
    let upstream_tcp =
        match super::forward_proxy::connect_upstream_for_mitm(host, port, config).await {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to connect to {host}:{port}: {e}");
                config.log_error(host, &format!("upstream connect failed: {e}"));
                return None;
            }
        };

    let extra = config.ca_cert_der.as_ref().map(std::slice::from_ref);
    let connector = tokio_rustls::TlsConnector::from(Arc::new(upstream_tls_config(extra)));
    let server_name = match rustls::pki_types::ServerName::try_from(host.to_string()) {
        Ok(sn) => sn,
        Err(e) => {
            warn!("Invalid server name {host}: {e}");
            return None;
        }
    };

    let upstream_tls = match connector.connect(server_name, upstream_tcp).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Upstream TLS handshake failed for {host}: {e}");
            config.log_error(host, &format!("upstream TLS failed: {e}"));
            return None;
        }
    };

    let is_h2 = upstream_tls
        .get_ref()
        .1
        .alpn_protocol()
        .is_some_and(|p| p == b"h2");
    let upstream_io = TokioIo::new(upstream_tls);

    if is_h2 {
        let (sender, conn) =
            match hyper::client::conn::http2::handshake(TokioExecutor::new(), upstream_io).await {
                Ok(pair) => pair,
                Err(e) => {
                    warn!("Upstream HTTP/2 handshake failed for {host}: {e}");
                    return None;
                }
            };
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                debug!("Upstream h2 connection ended: {e}");
            }
        });
        Some(UpstreamSender::Http2(sender))
    } else {
        let (sender, conn) = match hyper::client::conn::http1::handshake(upstream_io).await {
            Ok(pair) => pair,
            Err(e) => {
                warn!("Upstream HTTP/1.1 handshake failed for {host}: {e}");
                return None;
            }
        };
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                debug!("Upstream h1 connection ended: {e}");
            }
        });
        Some(UpstreamSender::Http1(sender))
    }
}

fn is_upgrade_request<B>(req: &Request<B>) -> bool {
    req.headers().get("upgrade").is_some()
        && req
            .headers()
            .get("connection")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(',')
                    .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
            })
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
) -> Result<Response<BoxBody>, std::convert::Infallible> {
    let client_upgrade = hyper::upgrade::on(&mut req);
    let req = prepare_forwarded_request(req, host, true);

    let mut sender = match connect_h1_upstream(host, port, config).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Upgrade upstream connection to {host} failed: {e}");
            return Ok(Response::builder()
                .status(hyper::StatusCode::BAD_GATEWAY)
                .body(full("Bad Gateway"))
                .expect("building 502"));
        }
    };

    let mut resp = match sender.send_request(req).await {
        Ok(r) => r,
        Err(e) => {
            warn!("Upgrade request to {host} failed: {e}");
            return Ok(Response::builder()
                .status(hyper::StatusCode::BAD_GATEWAY)
                .body(full("Bad Gateway"))
                .expect("building 502"));
        }
    };

    if resp.status() != hyper::StatusCode::SWITCHING_PROTOCOLS {
        return Ok(resp.map(|body| body.map_err(Into::into).boxed()));
    }

    let upstream_upgrade = hyper::upgrade::on(&mut resp);
    let host_owned = host.to_string();
    tokio::spawn(async move {
        let (client_io, upstream_io) = match tokio::join!(client_upgrade, upstream_upgrade) {
            (Ok(c), Ok(u)) => (c, u),
            (Err(e), _) | (_, Err(e)) => {
                debug!("Upgrade for {host_owned} failed: {e}");
                return;
            }
        };
        let mut client_io = TokioIo::new(client_io);
        let mut upstream_io = TokioIo::new(upstream_io);
        if let Err(e) = tokio::io::copy_bidirectional(&mut client_io, &mut upstream_io).await {
            debug!("Upgraded connection to {host_owned} ended: {e}");
        }
    });

    Ok(resp.map(|body| body.map_err(Into::into).boxed()))
}

async fn handle_request(
    req: Request<Incoming>,
    host: &str,
    port: u16,
    config: &AgentProxyConfig,
    sender: &Mutex<UpstreamSender>,
) -> Result<Response<BoxBody>, std::convert::Infallible> {
    let path = super::forward_proxy::strip_query(req.uri().path());
    let verdict = config.matcher.evaluate(host, path);

    if !verdict.allowed {
        tracing::info!("BLOCKED HTTPS {host}{path} - {}", verdict.reason);
        config.log_blocked(host, path, &verdict.reason);
        let resp = Response::builder()
            .status(hyper::StatusCode::FORBIDDEN)
            .header("connection", "close")
            .body(full("Forbidden"))
            .expect("building 403 response");
        return Ok(resp);
    }

    // HTTP/2 clients use Extended CONNECT (RFC 8441) instead of Upgrade headers,
    // so this only triggers for HTTP/1.1 clients.
    if is_upgrade_request(&req) {
        return handle_upgrade_request(req, host, port, config).await;
    }

    let req = prepare_forwarded_request(req, host, false);
    let mut upstream = sender.lock().await;
    match upstream.send_request(req).await {
        Ok(resp) => Ok(resp.map(|body| body.map_err(Into::into).boxed())),
        Err(e) => {
            warn!("Upstream request to {host} failed: {e}");
            let resp = Response::builder()
                .status(hyper::StatusCode::BAD_GATEWAY)
                .body(full("Bad Gateway"))
                .expect("building 502 response");
            Ok(resp)
        }
    }
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

    let hop_by_hop: &[&str] = if preserve_upgrade {
        &[
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
        ]
    } else {
        &[
            "connection",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
        ]
    };
    for name in hop_by_hop {
        parts.headers.remove(*name);
    }

    Request::from_parts(parts, body)
}

fn full(data: &'static str) -> BoxBody {
    Full::new(Bytes::from(data))
        .map_err(|never| match never {})
        .boxed()
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

    fn install_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
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

    #[test]
    fn detects_websocket_upgrade_request() {
        let req = Request::builder()
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .body(())
            .unwrap();
        assert!(is_upgrade_request(&req));
    }

    #[test]
    fn detects_upgrade_in_multi_value_connection() {
        let req = Request::builder()
            .header("upgrade", "websocket")
            .header("connection", "keep-alive, Upgrade")
            .body(())
            .unwrap();
        assert!(is_upgrade_request(&req));
    }

    #[test]
    fn rejects_request_without_upgrade_header() {
        let req = Request::builder()
            .header("connection", "keep-alive")
            .body(())
            .unwrap();
        assert!(!is_upgrade_request(&req));
    }

    #[test]
    fn rejects_upgrade_without_connection_token() {
        let req = Request::builder()
            .header("upgrade", "websocket")
            .header("connection", "keep-alive")
            .body(())
            .unwrap();
        assert!(!is_upgrade_request(&req));
    }
}
