//! HTTP plumbing shared by the plain-HTTP proxy and the MITM path.

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::body::Incoming;
use hyper::header::{CONNECTION, HeaderMap, HeaderName, HeaderValue, UPGRADE};
use hyper::upgrade::OnUpgrade;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tracing::debug;

pub type BoxBody =
    http_body_util::combinators::BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// Hop-by-hop headers removed before forwarding, besides `connection`,
/// `upgrade`, and whatever the `connection` header itself lists.
const HOP_BY_HOP: &[&str] = &[
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
];

/// Strip hop-by-hop headers (RFC 9110 §7.6.1) from a request or response.
///
/// With `preserve_upgrade`, `connection` and `upgrade` are kept so an
/// upgrade handshake can be forwarded.
pub fn strip_hop_by_hop(headers: &mut HeaderMap, preserve_upgrade: bool) {
    let listed: Vec<HeaderName> = header_tokens(headers, &CONNECTION)
        .filter_map(|token| HeaderName::from_bytes(token.as_bytes()).ok())
        .collect();
    for name in listed {
        if !(preserve_upgrade && name == UPGRADE) {
            headers.remove(name);
        }
    }
    for name in HOP_BY_HOP {
        headers.remove(*name);
    }
    if !preserve_upgrade {
        headers.remove(CONNECTION);
        headers.remove(UPGRADE);
        headers.remove("http2-settings");
    }
}

/// Whether an HTTP/1.1 request asks for a protocol upgrade.
///
/// HTTP/2 clients use Extended CONNECT (RFC 8441) instead of `Upgrade`
/// headers, so this only matches HTTP/1.1 requests.
pub fn is_upgrade_request<B>(req: &Request<B>) -> bool {
    req.headers().contains_key(UPGRADE)
        && header_tokens(req.headers(), &CONNECTION).any(|t| t.eq_ignore_ascii_case("upgrade"))
}

/// Whether a request asks to upgrade to WebSocket specifically.
pub fn is_websocket_upgrade<B>(req: &Request<B>) -> bool {
    is_upgrade_request(req)
        && header_tokens(req.headers(), &UPGRADE).any(|t| {
            let protocol = t.split_once('/').map_or(t, |(name, _)| name);
            protocol.trim().eq_ignore_ascii_case("websocket")
        })
}

/// Whether an upstream response switches to WebSocket and nothing else: a
/// `101` whose `Upgrade` header carries exactly one token, `websocket`, and
/// no field line that is not visible text.
pub fn is_websocket_switch<B>(resp: &Response<B>) -> bool {
    let mut tokens = header_tokens(resp.headers(), &UPGRADE);
    resp.status() == StatusCode::SWITCHING_PROTOCOLS
        && !has_invalid_lines(resp.headers(), &UPGRADE)
        && tokens
            .next()
            .is_some_and(|t| t.eq_ignore_ascii_case("websocket"))
        && tokens.next().is_none()
}

/// Whether a `Connection` or `Upgrade` field line is not visible ASCII text.
///
/// [`header_tokens`] skips such lines, so a message carrying one must be
/// rejected rather than evaluated with the line ignored.
pub fn has_invalid_token_lines(headers: &HeaderMap) -> bool {
    has_invalid_lines(headers, &CONNECTION) || has_invalid_lines(headers, &UPGRADE)
}

fn has_invalid_lines(headers: &HeaderMap, name: &HeaderName) -> bool {
    headers.get_all(name).iter().any(|v| v.to_str().is_err())
}

/// Comma-separated tokens across every field line of `name`, skipping lines
/// that are not visible ASCII text.
fn header_tokens<'a>(
    headers: &'a HeaderMap,
    name: &HeaderName,
) -> impl Iterator<Item = &'a str> + 'a {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// Return an upstream upgrade response to the client, splicing the two
/// upgraded connections once the upstream switched protocols.
pub fn relay_upgrade(
    client_upgrade: OnUpgrade,
    mut resp: Response<Incoming>,
    host: &str,
) -> Response<BoxBody> {
    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        let upstream_upgrade = hyper::upgrade::on(&mut resp);
        let host = host.to_string();
        tokio::spawn(async move {
            let (client_io, upstream_io) = match tokio::join!(client_upgrade, upstream_upgrade) {
                (Ok(c), Ok(u)) => (c, u),
                (Err(e), _) | (_, Err(e)) => {
                    debug!("Upgrade for {host} failed: {e}");
                    return;
                }
            };
            let mut client_io = TokioIo::new(client_io);
            let mut upstream_io = TokioIo::new(upstream_io);
            if let Err(e) = tokio::io::copy_bidirectional(&mut client_io, &mut upstream_io).await {
                debug!("Upgraded connection to {host} ended: {e}");
            }
        });
    }
    resp.map(|body| body.map_err(Into::into).boxed())
}

pub fn full(data: &'static str) -> BoxBody {
    Full::new(Bytes::from(data))
        .map_err(|never| match never {})
        .boxed()
}

/// A plain-text response with the given status.
pub fn text_response(status: StatusCode, body: &'static str) -> Response<BoxBody> {
    let mut resp = Response::new(full(body));
    *resp.status_mut() = status;
    resp
}

/// A plain-text response that also closes the client connection.
pub fn closing_response(status: StatusCode, body: &'static str) -> Response<BoxBody> {
    let mut resp = text_response(status, body);
    resp.headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn detects_upgrade_token_on_a_later_connection_line() {
        let req = Request::builder()
            .header("upgrade", "websocket")
            .header("connection", "keep-alive")
            .header("connection", "Upgrade")
            .body(())
            .unwrap();
        assert!(is_upgrade_request(&req));
        assert!(is_websocket_upgrade(&req));
    }

    #[test]
    fn websocket_upgrade_requires_websocket_token() {
        let h2c = Request::builder()
            .header("upgrade", "h2c")
            .header("connection", "Upgrade, HTTP2-Settings")
            .body(())
            .unwrap();
        assert!(is_upgrade_request(&h2c));
        assert!(!is_websocket_upgrade(&h2c));

        let ws = Request::builder()
            .header("upgrade", "foo, WebSocket")
            .header("connection", "upgrade")
            .body(())
            .unwrap();
        assert!(is_websocket_upgrade(&ws));
    }

    #[test]
    fn websocket_switch_requires_sole_websocket_token() {
        let switch = |upgrade: &'static str| {
            Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .header("upgrade", upgrade)
                .body(())
                .unwrap()
        };
        assert!(is_websocket_switch(&switch("WebSocket")));
        assert!(!is_websocket_switch(&switch("h2c")));
        assert!(!is_websocket_switch(&switch("websocket, h2c")));
        let ok = Response::builder()
            .header("upgrade", "websocket")
            .body(())
            .unwrap();
        assert!(!is_websocket_switch(&ok));
    }

    #[test]
    fn websocket_switch_rejects_obs_text_upgrade_line() {
        let mut resp = Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header("upgrade", "websocket")
            .body(())
            .unwrap();
        resp.headers_mut()
            .append(UPGRADE, HeaderValue::from_bytes(b"h2c\xff").unwrap());
        assert!(!is_websocket_switch(&resp));
    }

    #[test]
    fn detects_obs_text_connection_and_upgrade_lines() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
        headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
        assert!(!has_invalid_token_lines(&headers));

        let mut connection = headers.clone();
        connection.append(CONNECTION, HeaderValue::from_bytes(b"\xff").unwrap());
        assert!(has_invalid_token_lines(&connection));

        let mut upgrade = headers;
        upgrade.append(UPGRADE, HeaderValue::from_bytes(b"\xffh2c").unwrap());
        assert!(has_invalid_token_lines(&upgrade));
    }

    #[test]
    fn strips_headers_listed_on_any_connection_line() {
        let mut headers = HeaderMap::new();
        headers.append(CONNECTION, HeaderValue::from_static("keep-alive"));
        headers.append(CONNECTION, HeaderValue::from_static("x-private"));
        headers.insert("x-private", HeaderValue::from_static("1"));
        headers.insert("http2-settings", HeaderValue::from_static("AAA"));
        strip_hop_by_hop(&mut headers, false);
        assert!(headers.get("x-private").is_none());
        assert!(headers.get("http2-settings").is_none());
    }

    #[test]
    fn strips_connection_listed_and_fixed_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("close, x-private"));
        headers.insert("x-private", HeaderValue::from_static("1"));
        headers.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        headers.insert("proxy-authorization", HeaderValue::from_static("Basic x"));
        headers.insert("x-kept", HeaderValue::from_static("1"));
        strip_hop_by_hop(&mut headers, false);
        assert!(headers.get(CONNECTION).is_none());
        assert!(headers.get("x-private").is_none());
        assert!(headers.get("keep-alive").is_none());
        assert!(headers.get("proxy-authorization").is_none());
        assert_eq!(headers["x-kept"], "1");
    }

    #[test]
    fn preserve_upgrade_keeps_upgrade_handshake_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("Upgrade, x-private"));
        headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert("x-private", HeaderValue::from_static("1"));
        strip_hop_by_hop(&mut headers, true);
        assert_eq!(headers[CONNECTION], "Upgrade, x-private");
        assert_eq!(headers[UPGRADE], "websocket");
        assert!(headers.get("x-private").is_none());
    }
}
