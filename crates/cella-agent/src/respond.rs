//! Per-request rule decisions shared by the plain-HTTP proxy and the MITM
//! path: refuse the request, answer it with a configured response, or let the
//! caller forward it and set the rule's response headers on what comes back.

use std::time::Duration;

use cella_network::config::{ResponseSpec, status_forbids_body};
use cella_network::rules::{HeadersIndex, RespondRule, RulePayloads, RuleVerdict, VerdictAction};
use http_body_util::BodyExt;
use hyper::body::{Body, Buf, Bytes};
use hyper::header::{CONNECTION, CONTENT_LENGTH, EXPECT, HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode, Version};
use tracing::{debug, info, warn};

use crate::forward_proxy::strip_query;
use crate::proxy_config::AgentProxyConfig;
use crate::proxy_http::{BoxBody, GatewayError, closing_response, full_bytes, text_response};

/// Header SDKs read to tell an API response from a proxy's.
const REQUEST_ID: &str = "request-id";

/// Request body discarded before a configured response goes out; past this
/// the body is left unread and the connection closes after the response.
const MAX_DRAIN_BYTES: usize = 64 * 1024 * 1024;

/// Whether a request body was read to its end before the response went out.
#[derive(Debug, PartialEq, Eq)]
enum BodyRead {
    Complete,
    /// Left unread: the client waits on `100-continue`, or the body ran past
    /// the cap or the drain timeout.
    Unread,
    /// Reading it failed: the client went away or broke the body's framing.
    Failed,
}

/// Which proxy path a request arrived on; each refuses in its own shape.
#[derive(Debug, Clone, Copy)]
pub enum Channel {
    /// An absolute-form `http://` request to the forward proxy.
    PlainHttp,
    /// A request decrypted from an intercepted CONNECT tunnel.
    Mitm,
}

impl Channel {
    fn blocked_response(self) -> Response<BoxBody> {
        match self {
            Self::PlainHttp => text_response(StatusCode::FORBIDDEN, "Forbidden"),
            Self::Mitm => closing_response(StatusCode::FORBIDDEN, "Forbidden"),
        }
    }
}

/// Outcome of evaluating one request against the rules.
pub enum Decision<B> {
    /// Send this response; nothing goes upstream.
    Done(Response<BoxBody>),
    /// Forward the request and set the `allow` rule's headers, if it has
    /// any, on the response.
    Forward(Request<B>, Option<HeadersIndex>),
}

/// Evaluate `req` against the rules and log the outcome.
///
/// A blocked request gets the channel's 403. A request matching a `respond`
/// rule gets that rule's next response once its body has been discarded (see
/// [`respond`]).
pub async fn decide<B>(
    req: Request<B>,
    host: &str,
    config: &AgentProxyConfig,
    channel: Channel,
) -> Decision<B>
where
    B: Body + Unpin,
{
    let path = strip_query(req.uri().path());
    let verdict = config.matcher.evaluate(host, path);
    match verdict.action {
        VerdictAction::Allow { response_headers } => {
            config.log_allowed(host, path);
            Decision::Forward(req, response_headers)
        }
        VerdictAction::Block => {
            match channel {
                Channel::PlainHttp => info!("BLOCKED HTTP {host}{path} - {}", verdict.reason),
                Channel::Mitm => info!("BLOCKED HTTPS {host}{path} - {}", verdict.reason),
            }
            config.log_blocked(host, path, &verdict.reason);
            Decision::Done(channel.blocked_response())
        }
        VerdictAction::Respond(ref rule) => {
            Decision::Done(respond(req, rule, &verdict, host, config).await)
        }
    }
}

/// Answer `req`, which matched `rule`, with the rule's next response once its
/// body has been discarded (see [`discard_body`]), so a client still sending
/// a large body reads the response instead of a reset.
///
/// The rule's sequence advances only here, after the drain, so a request
/// whose body fails, because the client went away, takes no step. That
/// request gets a 400 instead.
async fn respond<B>(
    req: Request<B>,
    rule: &RespondRule,
    verdict: &RuleVerdict,
    host: &str,
    config: &AgentProxyConfig,
) -> Response<BoxBody>
where
    B: Body + Unpin,
{
    let (parts, body) = req.into_parts();
    let read = discard_body(&parts.headers, body, config.respond_drain_timeout).await;
    let path = strip_query(parts.uri.path());
    let mut resp = if read == BodyRead::Failed {
        debug!("Request body for {host}{path} failed; answered 400 instead of the rule");
        text_response(StatusCode::BAD_REQUEST, "Bad Request")
    } else {
        let prepared = config.prepared_rules.next_response(rule);
        let resp = respond_response(prepared, &parts.method, host, config);
        let status = resp.status().as_u16();
        info!("RESPONDED {host}{path} with {status} - {}", verdict.reason);
        let rule_display = verdict.matched_rule.as_deref().unwrap_or_default();
        config.log_responded(host, path, status, rule_display);
        resp
    };
    // What is left of an HTTP/1 body would be read as the next request.
    // HTTP/2 has no connection header: dropping the unread body resets just
    // this stream, with NO_ERROR once the response has gone out.
    if read != BodyRead::Complete
        && parts.version < Version::HTTP_2
        && let Err(e) = try_insert(
            resp.headers_mut(),
            &CONNECTION,
            &HeaderValue::from_static("close"),
        )
    {
        warn!("Response for {host} cannot close its connection: {e}");
    }
    resp
}

/// Build the response a `respond` rule configured.
///
/// Every response carries a `request-id`: the configured one, or a fresh
/// `req_cella_<uuid>` when none is configured. A response to `HEAD` has an
/// empty body, which hyper's HTTP/2 server would otherwise send. A spec the
/// host should have rejected yields a 502 and an error log line.
fn respond_response(
    prepared: &PreparedResponse,
    method: &Method,
    host: &str,
    config: &AgentProxyConfig,
) -> Response<BoxBody> {
    prepared.build(method).unwrap_or_else(|e| {
        warn!("Configured response for {host} is invalid: {e}");
        config.log_error(host, &format!("configured response invalid: {e}"));
        text_response(StatusCode::BAD_GATEWAY, "Bad Gateway")
    })
}

/// The configured responses and `allow` headers of a matcher's rules, parsed
/// once when the config loads so serving a request only clones them.
///
/// Built from the payloads the matcher moved out, keeping their positions, so
/// every index a verdict of that matcher carries has an entry here.
pub struct PreparedRules {
    /// Per `respond` rule, at its [`RespondRule::index`], its responses in
    /// serving order.
    responses: Vec<Vec<PreparedResponse>>,
    /// Per `allow` rule with headers, at its [`HeadersIndex`].
    headers: Vec<PreparedHeaders>,
}

impl PreparedRules {
    pub fn new(payloads: RulePayloads) -> Self {
        Self {
            responses: payloads
                .responses
                .into_iter()
                .map(|responses| responses.into_iter().map(PreparedResponse::new).collect())
                .collect(),
            headers: payloads
                .headers
                .iter()
                .map(|headers| PreparedHeaders::new(headers))
                .collect(),
        }
    }

    /// Advance `rule` and return the prepared response it serves now. `rule`
    /// is one of the matcher's these were prepared from.
    fn next_response(&self, rule: &RespondRule) -> &PreparedResponse {
        &self.responses[rule.index().get()][rule.next_index()]
    }

    /// The prepared headers of the `allow` rule at `index`.
    fn headers(&self, index: HeadersIndex) -> &PreparedHeaders {
        &self.headers[index.get()]
    }
}

/// A configured response, or why it cannot be served.
struct PreparedResponse(Result<ResponseParts, String>);

struct ResponseParts {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl PreparedResponse {
    fn new(spec: ResponseSpec) -> Self {
        Self(ResponseParts::new(spec))
    }

    fn build(&self, method: &Method) -> Result<Response<BoxBody>, String> {
        let parts = self.0.as_ref().map_err(Clone::clone)?;
        let is_head = method == Method::HEAD;
        let body = if is_head {
            Bytes::new()
        } else {
            parts.body.clone()
        };
        let mut resp = Response::new(full_bytes(body));
        *resp.status_mut() = parts.status;
        *resp.headers_mut() = parts.headers.clone();
        // hyper's HTTP/1 and HTTP/2 servers add no `content-length` for the
        // empty HEAD body but keep one set here, so HEAD reports the GET length.
        if is_head && !status_forbids_body(parts.status.as_u16()) {
            try_insert(
                resp.headers_mut(),
                &CONTENT_LENGTH,
                &HeaderValue::from(parts.body.len()),
            )?;
        }
        if !parts.headers.contains_key(REQUEST_ID) {
            let id = format!("req_cella_{}", uuid::Uuid::new_v4().simple());
            let id = HeaderValue::try_from(id).map_err(|e| format!("generated request-id: {e}"))?;
            try_insert(
                resp.headers_mut(),
                &HeaderName::from_static(REQUEST_ID),
                &id,
            )?;
        }
        Ok(resp)
    }
}

impl ResponseParts {
    fn new(spec: ResponseSpec) -> Result<Self, String> {
        let status = StatusCode::from_u16(spec.status)
            .map_err(|e| format!("status {}: {e}", spec.status))?;
        let mut headers = HeaderMap::new();
        for (name, value) in &spec.headers {
            let (name, value) = parse_header(name, value)?;
            try_insert(&mut headers, &name, &value)?;
        }
        Ok(Self {
            status,
            headers,
            body: Bytes::from(spec.body),
        })
    }
}

/// An `allow` rule's response headers, each parsed or the reason it cannot be.
struct PreparedHeaders(Vec<Result<(HeaderName, HeaderValue), String>>);

impl PreparedHeaders {
    fn new(headers: &[(String, String)]) -> Self {
        Self(
            headers
                .iter()
                .map(|(name, value)| parse_header(name, value))
                .collect(),
        )
    }
}

/// The response for a forwarded request: the upstream's, with the `allow`
/// rule's headers set on it, or the proxy's own error reply untouched.
pub fn forwarded_response(
    forwarded: Result<Response<BoxBody>, GatewayError>,
    headers: Option<HeadersIndex>,
    host: &str,
    config: &AgentProxyConfig,
) -> Response<BoxBody> {
    match forwarded {
        Ok(mut resp) => {
            if let Some(index) = headers {
                let headers = config.prepared_rules.headers(index);
                inject_response_headers(&mut resp, headers, host, config);
            }
            resp
        }
        Err(e) => e.into_response(),
    }
}

/// Set an `allow` rule's headers on a forwarded response, replacing any value
/// the upstream sent for the same name.
fn inject_response_headers(
    resp: &mut Response<BoxBody>,
    headers: &PreparedHeaders,
    host: &str,
    config: &AgentProxyConfig,
) {
    for header in &headers.0 {
        let inserted = match header {
            Ok((name, value)) => try_insert(resp.headers_mut(), name, value),
            Err(e) => Err(e.clone()),
        };
        if let Err(e) = inserted {
            warn!("Response header for {host} is invalid: {e}");
            config.log_error(host, &format!("response header invalid: {e}"));
        }
    }
}

fn parse_header(name: &str, value: &str) -> Result<(HeaderName, HeaderValue), String> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|e| format!("header name {name:?}: {e}"))?;
    let header_value = HeaderValue::from_bytes(value.as_bytes())
        .map_err(|e| format!("value of header {name:?}: {e}"))?;
    Ok((header_name, header_value))
}

/// Set `name: value` in `headers`, replacing any earlier value, without
/// panicking when the map is full.
fn try_insert(
    headers: &mut HeaderMap,
    name: &HeaderName,
    value: &HeaderValue,
) -> Result<(), String> {
    headers
        .try_insert(name.clone(), value.clone())
        .map_err(|e| format!("header {name}: {e}"))?;
    Ok(())
}

/// Discard a request's body before a configured response goes out.
///
/// A client waiting on `Expect: 100-continue` has not sent its body, so it is
/// left unread, which also keeps hyper from sending `100 Continue`. Any other
/// body is read for at most `timeout`. A client that goes away mid-body
/// surfaces as a failed read.
async fn discard_body<B>(headers: &HeaderMap, body: B, timeout: Duration) -> BodyRead
where
    B: Body + Unpin,
{
    if expects_continue(headers) {
        return BodyRead::Unread;
    }
    tokio::time::timeout(timeout, drain_body(body))
        .await
        .unwrap_or(BodyRead::Unread)
}

fn expects_continue(headers: &HeaderMap) -> bool {
    headers.get_all(EXPECT).iter().any(|value| {
        value
            .as_bytes()
            .trim_ascii()
            .eq_ignore_ascii_case(b"100-continue")
    })
}

/// Read and discard a request body, frame by frame, up to [`MAX_DRAIN_BYTES`].
async fn drain_body<B>(mut body: B) -> BodyRead
where
    B: Body + Unpin,
{
    let mut seen = 0_usize;
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            return BodyRead::Failed;
        };
        if let Some(data) = frame.data_ref() {
            seen = seen.saturating_add(data.remaining());
            if seen > MAX_DRAIN_BYTES {
                return BodyRead::Unread;
            }
        }
    }
    BodyRead::Complete
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use http_body_util::{Empty, StreamBody};
    use hyper::body::Frame;
    use tempfile::TempDir;
    use tracing::instrument::WithSubscriber;

    use super::*;

    fn test_config() -> (TempDir, AgentProxyConfig) {
        let dir = TempDir::new().unwrap();
        let json = serde_json::json!({
            "listen_port": 0,
            "mode": "denylist",
            "rules": [],
            "log_path": dir.path().join("proxy.log"),
        })
        .to_string();
        let config = AgentProxyConfig::from_json(&json).unwrap();
        (dir, config)
    }

    fn prepared(status: u16, headers: &[(&str, &str)], body: &str) -> PreparedResponse {
        PreparedResponse::new(ResponseSpec {
            status,
            headers: headers
                .iter()
                .map(|(n, v)| ((*n).to_string(), (*v).to_string()))
                .collect::<BTreeMap<_, _>>(),
            body: body.to_string(),
        })
    }

    fn request_ids(resp: &Response<BoxBody>) -> Vec<String> {
        resp.headers()
            .get_all(REQUEST_ID)
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect()
    }

    async fn body_text(resp: Response<BoxBody>) -> String {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn serves_status_headers_and_body() {
        let (_dir, config) = test_config();
        let spec = prepared(
            529,
            &[("content-type", "application/json"), ("retry-after", "0")],
            r#"{"type":"error"}"#,
        );
        let resp = respond_response(&spec, &Method::GET, "api.example.com", &config);
        assert_eq!(resp.status().as_u16(), 529);
        assert_eq!(resp.headers()["content-type"], "application/json");
        assert_eq!(resp.headers()["retry-after"], "0");
        assert!(resp.headers().get("connection").is_none());
        assert_eq!(body_text(resp).await, r#"{"type":"error"}"#);
    }

    /// A `tracing` writer appending to a shared buffer.
    #[derive(Clone, Default)]
    struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn blocked_request_log_line_has_no_userinfo_or_query() {
        let dir = TempDir::new().unwrap();
        let json = serde_json::json!({
            "listen_port": 0,
            "mode": "denylist",
            "rules": [{"domain": "h.example", "action": "block"}],
            "log_path": dir.path().join("proxy.log"),
        })
        .to_string();
        let config = AgentProxyConfig::from_json(&json).unwrap();

        for (channel, expected) in [
            (Channel::PlainHttp, "BLOCKED HTTP h.example/v1/p - "),
            (Channel::Mitm, "BLOCKED HTTPS h.example/v1/p - "),
        ] {
            let captured = Captured::default();
            let writer = captured.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .without_time()
                .finish();
            let req = Request::get("http://user:secret@h.example/v1/p?token=abc")
                .body(Empty::<Bytes>::new())
                .unwrap();
            let decision = decide(req, "h.example", &config, channel)
                .with_subscriber(subscriber)
                .await;
            assert!(matches!(decision, Decision::Done(_)));
            let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
            assert!(log.contains(expected), "{log}");
            assert!(!log.contains("secret"), "{log}");
            assert!(!log.contains("token"), "{log}");
        }
    }

    #[tokio::test]
    async fn head_response_reports_the_body_length_without_the_body() {
        let (_dir, config) = test_config();
        let resp = respond_response(&prepared(529, &[], "busy"), &Method::HEAD, "a", &config);
        assert_eq!(resp.headers()["content-length"], "4");
        assert_eq!(body_text(resp).await, "");

        for status in [204, 205, 304] {
            let resp = respond_response(&prepared(status, &[], ""), &Method::HEAD, "a", &config);
            assert!(resp.headers().get("content-length").is_none(), "{status}");
        }
    }

    #[test]
    fn adds_no_content_type_of_its_own() {
        let (_dir, config) = test_config();
        let resp = respond_response(
            &prepared(200, &[], "plain"),
            &Method::GET,
            "api.example.com",
            &config,
        );
        assert!(resp.headers().get("content-type").is_none());
    }

    #[test]
    fn generates_a_fresh_request_id_per_response() {
        let (_dir, config) = test_config();
        let spec = prepared(200, &[], "");
        let first = request_ids(&respond_response(
            &spec,
            &Method::GET,
            "api.example.com",
            &config,
        ));
        let second = request_ids(&respond_response(
            &spec,
            &Method::GET,
            "api.example.com",
            &config,
        ));
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert!(first[0].starts_with("req_cella_"), "got {first:?}");
        assert_eq!(first[0].len(), "req_cella_".len() + 32, "got {first:?}");
        assert_ne!(first, second);
    }

    #[test]
    fn keeps_a_configured_request_id_in_any_case() {
        let (_dir, config) = test_config();
        let spec = prepared(200, &[("Request-ID", "req_fixed")], "");
        let resp = respond_response(&spec, &Method::GET, "api.example.com", &config);
        assert_eq!(request_ids(&resp), ["req_fixed"]);
    }

    #[test]
    fn invalid_header_name_yields_502_and_logs() {
        let (dir, config) = test_config();
        let resp = respond_response(
            &prepared(200, &[("bad name", "x")], ""),
            &Method::GET,
            "a.example",
            &config,
        );
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let log = std::fs::read_to_string(dir.path().join("proxy.log")).unwrap();
        assert!(
            log.contains("\tERROR\ta.example\tconfigured response invalid"),
            "{log}"
        );
    }

    #[test]
    fn invalid_header_value_yields_502() {
        let (_dir, config) = test_config();
        let resp = respond_response(
            &prepared(200, &[("x-a", "line\nbreak")], ""),
            &Method::GET,
            "a",
            &config,
        );
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn invalid_status_yields_502() {
        let (_dir, config) = test_config();
        let resp = respond_response(&prepared(1000, &[], ""), &Method::GET, "a", &config);
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn injection_overwrites_and_adds_no_request_id() {
        let (_dir, config) = test_config();
        let mut resp = text_response(StatusCode::OK, "ok");
        resp.headers_mut()
            .append("x-test-run", HeaderValue::from_static("upstream"));
        resp.headers_mut()
            .append("x-test-run", HeaderValue::from_static("upstream-2"));
        let headers = [("X-Test-Run".to_string(), "1".to_string())];
        inject_response_headers(&mut resp, &PreparedHeaders::new(&headers), "a", &config);
        let values: Vec<_> = resp.headers().get_all("x-test-run").iter().collect();
        assert_eq!(values, [HeaderValue::from_static("1")]);
        assert!(resp.headers().get(REQUEST_ID).is_none());
    }

    #[test]
    fn injection_skips_an_invalid_header() {
        let (_dir, config) = test_config();
        let mut resp = text_response(StatusCode::OK, "ok");
        let headers = [
            ("bad name".to_string(), "1".to_string()),
            ("x-good".to_string(), "1".to_string()),
        ];
        inject_response_headers(&mut resp, &PreparedHeaders::new(&headers), "a", &config);
        assert_eq!(resp.headers()["x-good"], "1");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn matcher_responses_and_headers_are_prepared_at_load() {
        let json = serde_json::json!({
            "listen_port": 0,
            "mode": "denylist",
            "rules": [
                {"domain": "a.example", "action": "allow",
                 "response_headers": {"x-a": "1"}},
                {"domain": "r.example", "action": "respond",
                 "responses": [{"status": 529}, {"status": 200, "body": "ok"}]},
                {"domain": "b.example", "action": "allow",
                 "response_headers": {"x-b": "2"}},
                {"domain": "s.example", "action": "respond",
                 "responses": [{"status": 418, "body": "teapot"}]},
            ],
        })
        .to_string();
        let config = AgentProxyConfig::from_json(&json).unwrap();

        let served = |domain: &str| {
            let VerdictAction::Respond(rule) = config.matcher.evaluate(domain, "/").action else {
                panic!("expected a respond verdict for {domain}");
            };
            let parts = config
                .prepared_rules
                .next_response(&rule)
                .0
                .as_ref()
                .unwrap();
            (parts.status.as_u16(), parts.body.clone())
        };
        assert_eq!(served("r.example"), (529, Bytes::new()));
        assert_eq!(served("s.example"), (418, Bytes::from("teapot")));
        assert_eq!(served("r.example"), (200, Bytes::from("ok")));
        assert_eq!(served("r.example"), (200, Bytes::from("ok")));

        for (domain, name, value) in [("a.example", "x-a", "1"), ("b.example", "x-b", "2")] {
            let VerdictAction::Allow {
                response_headers: Some(index),
            } = config.matcher.evaluate(domain, "/").action
            else {
                panic!("expected allow with headers for {domain}");
            };
            let headers = &config.prepared_rules.headers(index).0;
            let [Ok((parsed_name, parsed_value))] = headers.as_slice() else {
                panic!("expected one parsed header for {domain}");
            };
            assert_eq!(parsed_name, name);
            assert_eq!(parsed_value, value);
        }
    }

    /// More distinct names than a `HeaderMap` can hold.
    fn too_many_headers() -> Vec<(String, String)> {
        (0..40_000)
            .map(|i| (format!("x-h{i}"), "1".to_string()))
            .collect()
    }

    #[test]
    fn overfull_response_yields_502_and_logs() {
        let (dir, config) = test_config();
        let spec = PreparedResponse::new(ResponseSpec {
            status: 200,
            headers: too_many_headers().into_iter().collect(),
            body: String::new(),
        });
        let resp = respond_response(&spec, &Method::GET, "a.example", &config);
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let log = std::fs::read_to_string(dir.path().join("proxy.log")).unwrap();
        assert!(
            log.contains("\tERROR\ta.example\tconfigured response invalid"),
            "{log}"
        );
    }

    #[test]
    fn injection_past_header_map_capacity_skips_and_logs() {
        let (dir, config) = test_config();
        let mut resp = text_response(StatusCode::OK, "ok");
        inject_response_headers(
            &mut resp,
            &PreparedHeaders::new(&too_many_headers()),
            "a.example",
            &config,
        );
        assert_eq!(resp.status(), StatusCode::OK);
        let log = std::fs::read_to_string(dir.path().join("proxy.log")).unwrap();
        assert!(
            log.contains("\tERROR\ta.example\tresponse header invalid"),
            "{log}"
        );
    }

    /// A config whose rule for `r.example` answers 529, then 418, then 200.
    fn sequence_config() -> (TempDir, AgentProxyConfig) {
        let dir = TempDir::new().unwrap();
        let json = serde_json::json!({
            "listen_port": 0,
            "mode": "denylist",
            "rules": [{"domain": "r.example", "action": "respond",
                       "responses": [{"status": 529}, {"status": 418}, {"status": 200}]}],
            "log_path": dir.path().join("proxy.log"),
        })
        .to_string();
        let config = AgentProxyConfig::from_json(&json).unwrap();
        (dir, config)
    }

    /// The status `decide` answers `req` to `r.example` with.
    async fn answered<B: Body + Unpin>(req: Request<B>, config: &AgentProxyConfig) -> u16 {
        match decide(req, "r.example", config, Channel::PlainHttp).await {
            Decision::Done(resp) => resp.status().as_u16(),
            Decision::Forward(..) => panic!("request was forwarded"),
        }
    }

    fn bodiless(method: Method) -> Request<Empty<Bytes>> {
        Request::builder()
            .method(method)
            .uri("http://r.example/v1/messages")
            .body(Empty::new())
            .unwrap()
    }

    #[tokio::test]
    async fn request_whose_body_fails_mid_drain_takes_no_step() {
        let (_dir, config) = sequence_config();
        let frames = [
            Ok(Frame::data(Bytes::from_static(b"partial"))),
            Err(std::io::Error::other("client went away")),
        ];
        let failing = Request::post("http://r.example/v1/messages")
            .body(StreamBody::new(tokio_stream::iter(frames)))
            .unwrap();
        assert_eq!(answered(failing, &config).await, 400);
        assert_eq!(answered(bodiless(Method::POST), &config).await, 529);
        assert_eq!(answered(bodiless(Method::POST), &config).await, 418);
    }

    #[tokio::test]
    async fn every_method_takes_a_step() {
        let (_dir, config) = sequence_config();
        let served = [
            answered(bodiless(Method::HEAD), &config).await,
            answered(bodiless(Method::OPTIONS), &config).await,
            answered(bodiless(Method::GET), &config).await,
        ];
        assert_eq!(served, [529, 418, 200]);
    }

    #[tokio::test]
    async fn drain_stops_after_the_cap() {
        const CHUNK: usize = 1024 * 1024;
        let polled = Arc::new(AtomicUsize::new(0));
        let counter = polled.clone();
        let frames = std::iter::repeat_with(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok::<_, Infallible>(Frame::data(Bytes::from(vec![0_u8; CHUNK])))
        });
        let read = drain_body(StreamBody::new(tokio_stream::iter(frames))).await;
        assert_eq!(read, BodyRead::Unread);
        assert_eq!(polled.load(Ordering::SeqCst), MAX_DRAIN_BYTES / CHUNK + 1);
    }

    #[tokio::test]
    async fn drain_reads_a_body_to_its_end() {
        let polled = Arc::new(AtomicUsize::new(0));
        let counter = polled.clone();
        let frames = std::iter::repeat_with(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok::<_, Infallible>(Frame::data(Bytes::from_static(b"chunk")))
        })
        .take(3);
        let read = drain_body(StreamBody::new(tokio_stream::iter(frames))).await;
        assert_eq!(read, BodyRead::Complete);
        assert_eq!(polled.load(Ordering::SeqCst), 3);
    }
}
