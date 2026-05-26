//! Agent-side credential tunnel relay.
//!
//! Connects to the daemon's credential proxy, sends the HTTP request
//! envelope, and reads the response envelope + streamed body chunks.

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::warn;

use crate::proxy_config::CredentialRoute;

type BoxBody =
    http_body_util::combinators::BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

const MAX_REQUEST_BODY: usize = 256 * 1024 * 1024;

/// JSON envelope for an HTTP request sent through the credential tunnel.
#[derive(Debug, serde::Serialize)]
struct HttpRequestEnvelope {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body_len: u32,
}

/// JSON envelope for an HTTP response received from the daemon.
#[derive(Debug, serde::Deserialize)]
struct HttpResponseEnvelope {
    status: u16,
    headers: Vec<(String, String)>,
}

/// Tunnel an intercepted HTTPS request through the daemon credential proxy.
///
/// Returns an HTTP response that can be sent back to the in-container client.
pub async fn tunnel_request(
    req: hyper::Request<hyper::body::Incoming>,
    host: &str,
    route: &CredentialRoute,
    daemon_addr: &str,
    daemon_token: &str,
    container_name: &str,
    container_nonce: Option<&str>,
) -> hyper::Response<BoxBody> {
    match tunnel_request_inner(
        req,
        host,
        route,
        daemon_addr,
        daemon_token,
        container_name,
        container_nonce,
    )
    .await
    {
        Ok(resp) => resp,
        Err(e) => {
            warn!("Credential tunnel to daemon failed: {e}");
            hyper::Response::builder()
                .status(502)
                .body(full("Credential proxy unavailable"))
                .expect("building 502 response")
        }
    }
}

async fn tunnel_request_inner(
    req: hyper::Request<hyper::body::Incoming>,
    host: &str,
    route: &CredentialRoute,
    daemon_addr: &str,
    daemon_token: &str,
    container_name: &str,
    container_nonce: Option<&str>,
) -> Result<hyper::Response<BoxBody>, Box<dyn std::error::Error + Send + Sync>> {
    let (parts, body) = req.into_parts();

    let body_bytes = body.collect().await?.to_bytes();

    if body_bytes.len() > MAX_REQUEST_BODY {
        return Ok(hyper::Response::builder()
            .status(413)
            .body(full("Request body too large"))?);
    }

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

    let envelope = HttpRequestEnvelope {
        method: parts.method.to_string(),
        uri: uri_str,
        headers,
        body_len: u32::try_from(body_bytes.len()).unwrap_or(u32::MAX),
    };

    let stream = TcpStream::connect(daemon_addr).await?;
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    let request_id = uuid::Uuid::new_v4().to_string();
    let handshake = cella_protocol::CredentialProxyHandshake {
        auth_token: daemon_token.to_string(),
        container_name: container_name.to_string(),
        request_id: request_id.clone(),
        domain: host.to_string(),
        provider_id: route.provider_id.clone(),
        container_nonce: container_nonce.map(String::from),
        trace_id: Some(format!("cred-{}", &request_id[..8])),
    };
    let mut hs_json = serde_json::to_string(&handshake)?;
    hs_json.push('\n');
    writer.write_all(hs_json.as_bytes()).await?;

    let mut env_json = serde_json::to_string(&envelope)?;
    env_json.push('\n');
    writer.write_all(env_json.as_bytes()).await?;

    if !body_bytes.is_empty() {
        writer.write_all(&body_bytes).await?;
    }
    writer.flush().await?;

    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).await?;
    let resp_envelope: HttpResponseEnvelope = serde_json::from_str(resp_line.trim())?;

    let (tx, rx) = tokio::sync::mpsc::channel::<
        Result<hyper::body::Frame<Bytes>, Box<dyn std::error::Error + Send + Sync>>,
    >(16);

    tokio::spawn(async move {
        loop {
            let mut len_buf = [0u8; 4];
            if reader.read_exact(&mut len_buf).await.is_err() {
                break;
            }
            let chunk_len = u32::from_be_bytes(len_buf) as usize;
            if chunk_len == 0 {
                break;
            }
            let mut chunk = vec![0u8; chunk_len];
            if reader.read_exact(&mut chunk).await.is_err() {
                break;
            }
            if tx
                .send(Ok(hyper::body::Frame::data(Bytes::from(chunk))))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let body = http_body_util::StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(rx));

    let mut builder = hyper::Response::builder().status(resp_envelope.status);
    for (key, value) in &resp_envelope.headers {
        builder = builder.header(key.as_str(), value.as_str());
    }

    Ok(builder.body(body.boxed())?)
}

fn full(s: &str) -> BoxBody {
    Full::new(Bytes::from(s.to_string()))
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_envelope_serialization() {
        let env = HttpRequestEnvelope {
            method: "POST".to_string(),
            uri: "/v1/messages".to_string(),
            headers: vec![("x-api-key".to_string(), "pt-abc".to_string())],
            body_len: 100,
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"method\":\"POST\""));
        assert!(json.contains("\"body_len\":100"));
    }

    #[test]
    fn response_envelope_deserialization() {
        let json = r#"{"status":200,"headers":[["content-type","application/json"]]}"#;
        let env: HttpResponseEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.status, 200);
        assert_eq!(env.headers.len(), 1);
    }
}
