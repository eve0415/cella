//! Daemon-side credential proxy handler.
//!
//! Accepts credential proxy connections from the agent, validates
//! phantom tokens, resolves real credentials, makes upstream HTTPS
//! requests, and streams responses back through the tunnel.

use std::sync::Arc;

use futures_util::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::credential_resolver::{self, ProviderMeta, ResolvedCredential};
use crate::phantom_registry::PhantomRegistry;

/// JSON envelope for an HTTP request sent through the credential tunnel.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct HttpRequestEnvelope {
    pub method: String,
    pub uri: String,
    pub headers: Vec<(String, String)>,
    pub body_len: u32,
}

/// JSON envelope for an HTTP response sent back through the tunnel.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct HttpResponseEnvelope {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

/// Provider metadata needed by the proxy to resolve and inject credentials.
#[derive(Debug, Clone)]
pub struct ProxyProviderMeta {
    pub env_var: String,
    pub header: String,
    pub prefix: String,
    pub domain: String,
}

/// Shared state for credential proxy connections.
pub struct CredentialProxyContext {
    pub phantom_registry: Arc<Mutex<PhantomRegistry>>,
    pub providers: Arc<Mutex<std::collections::HashMap<String, ProxyProviderMeta>>>,
}

/// Handle a credential proxy connection after the handshake has been parsed.
///
/// # Errors
///
/// Returns error on I/O or protocol failures.
pub async fn handle_credential_proxy(
    handshake: cella_protocol::CredentialProxyHandshake,
    reader: BufReader<tokio::io::ReadHalf<tokio::net::TcpStream>>,
    mut writer: tokio::io::WriteHalf<tokio::net::TcpStream>,
    phantom_registry: &Arc<Mutex<PhantomRegistry>>,
) -> Result<(), crate::CellaDaemonError> {
    let result =
        handle_credential_proxy_inner(&handshake, reader, &mut writer, phantom_registry).await;

    if let Err(ref e) = result {
        warn!(
            "Credential proxy error for {} (provider={}): {e}",
            handshake.container_name, handshake.provider_id
        );
        let error_resp = HttpResponseEnvelope {
            status: 502,
            headers: vec![],
        };
        let _ = write_response_envelope(&mut writer, &error_resp).await;
        let _ = write_body_end(&mut writer).await;
    }

    result
}

async fn handle_credential_proxy_inner(
    handshake: &cella_protocol::CredentialProxyHandshake,
    mut reader: BufReader<tokio::io::ReadHalf<tokio::net::TcpStream>>,
    writer: &mut tokio::io::WriteHalf<tokio::net::TcpStream>,
    phantom_registry: &Arc<Mutex<PhantomRegistry>>,
) -> Result<(), crate::CellaDaemonError> {
    let mut req_line = String::new();
    reader
        .read_line(&mut req_line)
        .await
        .map_err(|e| crate::CellaDaemonError::Socket {
            message: format!("credential proxy: read request envelope: {e}"),
        })?;

    let req_envelope: HttpRequestEnvelope =
        serde_json::from_str(req_line.trim()).map_err(|e| crate::CellaDaemonError::Protocol {
            message: format!("credential proxy: invalid request envelope: {e}"),
        })?;

    let body = if req_envelope.body_len > 0 {
        let mut buf = vec![0u8; req_envelope.body_len as usize];
        reader
            .read_exact(&mut buf)
            .await
            .map_err(|e| crate::CellaDaemonError::Socket {
                message: format!("credential proxy: read body: {e}"),
            })?;
        buf
    } else {
        Vec::new()
    };

    let resolved = validate_and_resolve(handshake, &req_envelope, phantom_registry).await;

    let Some((cred, phantom_token)) = resolved else {
        write_response_envelope(
            writer,
            &HttpResponseEnvelope {
                status: 403,
                headers: vec![],
            },
        )
        .await?;
        write_body_end(writer).await?;
        return Ok(());
    };

    let upstream_resp = make_upstream_request(
        &req_envelope,
        &body,
        &handshake.domain,
        &cred,
        &phantom_token,
    )
    .await?;

    let status = stream_upstream_response(upstream_resp, writer).await?;

    info!(
        "CRED_PROXY {} {} {} -> {status}",
        handshake.container_name, handshake.provider_id, req_envelope.uri
    );

    Ok(())
}

async fn validate_and_resolve(
    handshake: &cella_protocol::CredentialProxyHandshake,
    req_envelope: &HttpRequestEnvelope,
    phantom_registry: &Arc<Mutex<PhantomRegistry>>,
) -> Option<(ResolvedCredential, String)> {
    let phantom_token = extract_phantom_token(&req_envelope.headers, &handshake.provider_id)?;

    let provider_id = phantom_registry
        .lock()
        .await
        .lookup(&handshake.container_name, &phantom_token)
        .map(String::from)?;

    if provider_id != handshake.provider_id {
        warn!(
            "Credential proxy: provider mismatch (handshake={}, resolved={provider_id})",
            handshake.provider_id,
        );
        return None;
    }

    let meta = phantom_registry
        .lock()
        .await
        .get_provider_meta(&handshake.container_name, &provider_id)
        .map_or_else(
            || build_provider_meta(&req_envelope.headers, &handshake.provider_id),
            |m| ProviderMeta {
                env_var: m.env_var.clone(),
                header: m.header.clone(),
                prefix: m.prefix.clone(),
            },
        );

    let cred = credential_resolver::resolve_credential(&provider_id, &meta)?;
    Some((cred, phantom_token))
}

async fn stream_upstream_response(
    resp: reqwest::Response,
    writer: &mut tokio::io::WriteHalf<tokio::net::TcpStream>,
) -> Result<u16, crate::CellaDaemonError> {
    let status = resp.status().as_u16();
    let resp_headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    write_response_envelope(
        writer,
        &HttpResponseEnvelope {
            status,
            headers: resp_headers,
        },
    )
    .await?;

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => write_body_chunk(writer, &bytes).await?,
            Err(e) => {
                warn!("Credential proxy: upstream read error: {e}");
                break;
            }
        }
    }
    write_body_end(writer).await?;

    Ok(status)
}

fn extract_phantom_token(headers: &[(String, String)], provider_id: &str) -> Option<String> {
    let header_name = match provider_id {
        "anthropic" => "x-api-key",
        "gemini" => "x-goog-api-key",
        _ => "authorization",
    };

    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(header_name))
        .map(|(_, v)| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
                .or_else(|| v.strip_prefix("token "))
                .or_else(|| v.strip_prefix("Token "))
                .unwrap_or(v)
                .to_string()
        })
}

fn build_provider_meta(headers: &[(String, String)], provider_id: &str) -> ProviderMeta {
    use cella_env::credential_providers::CREDENTIAL_PROVIDERS;

    if let Some(builtin) = CREDENTIAL_PROVIDERS.iter().find(|p| p.id == provider_id) {
        return ProviderMeta {
            env_var: builtin.env_var.to_string(),
            header: builtin.header.to_string(),
            prefix: builtin.prefix.to_string(),
        };
    }

    let header_name = headers
        .iter()
        .find(|(k, _)| {
            k.eq_ignore_ascii_case("authorization")
                || k.eq_ignore_ascii_case("x-api-key")
                || k.eq_ignore_ascii_case("x-goog-api-key")
        })
        .map_or_else(|| "Authorization".to_string(), |(k, _)| k.clone());

    ProviderMeta {
        env_var: format!("{}_API_KEY", provider_id.to_uppercase()),
        header: header_name,
        prefix: String::new(),
    }
}

async fn make_upstream_request(
    envelope: &HttpRequestEnvelope,
    body: &[u8],
    domain: &str,
    credential: &ResolvedCredential,
    phantom_token: &str,
) -> Result<reqwest::Response, crate::CellaDaemonError> {
    let url = format!("https://{domain}{}", envelope.uri);
    let client = reqwest::Client::new();

    let method: reqwest::Method = envelope.method.parse().unwrap_or(reqwest::Method::GET);

    let mut builder = client.request(method, &url);

    for (key, value) in &envelope.headers {
        if key.eq_ignore_ascii_case(&credential.header_name) {
            continue;
        }
        if key.eq_ignore_ascii_case("host") {
            continue;
        }
        let clean_value = value.replace(phantom_token, "");
        if clean_value.trim().is_empty() {
            continue;
        }
        builder = builder.header(key.as_str(), clean_value);
    }

    builder = builder.header(&credential.header_name, &credential.header_value);

    if !body.is_empty() {
        builder = builder.body(body.to_vec());
    }

    builder
        .send()
        .await
        .map_err(|e| crate::CellaDaemonError::Socket {
            message: format!("credential proxy: upstream request failed: {e}"),
        })
}

async fn write_response_envelope(
    writer: &mut tokio::io::WriteHalf<tokio::net::TcpStream>,
    envelope: &HttpResponseEnvelope,
) -> Result<(), crate::CellaDaemonError> {
    let mut json =
        serde_json::to_string(envelope).map_err(|e| crate::CellaDaemonError::Protocol {
            message: format!("serialize response envelope: {e}"),
        })?;
    json.push('\n');
    writer
        .write_all(json.as_bytes())
        .await
        .map_err(|e| crate::CellaDaemonError::Socket {
            message: format!("write response envelope: {e}"),
        })
}

async fn write_body_chunk(
    writer: &mut tokio::io::WriteHalf<tokio::net::TcpStream>,
    data: &[u8],
) -> Result<(), crate::CellaDaemonError> {
    let len = u32::try_from(data.len()).unwrap_or(u32::MAX);
    writer
        .write_all(&len.to_be_bytes())
        .await
        .map_err(|e| crate::CellaDaemonError::Socket {
            message: format!("write body chunk len: {e}"),
        })?;
    writer
        .write_all(data)
        .await
        .map_err(|e| crate::CellaDaemonError::Socket {
            message: format!("write body chunk data: {e}"),
        })
}

async fn write_body_end(
    writer: &mut tokio::io::WriteHalf<tokio::net::TcpStream>,
) -> Result<(), crate::CellaDaemonError> {
    writer
        .write_all(&0u32.to_be_bytes())
        .await
        .map_err(|e| crate::CellaDaemonError::Socket {
            message: format!("write body end: {e}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_envelope_roundtrip() {
        let env = HttpRequestEnvelope {
            method: "POST".to_string(),
            uri: "/v1/messages".to_string(),
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("x-api-key".to_string(), "pt-abc-123".to_string()),
            ],
            body_len: 42,
        };
        let json = serde_json::to_string(&env).unwrap();
        let decoded: HttpRequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.method, "POST");
        assert_eq!(decoded.uri, "/v1/messages");
        assert_eq!(decoded.headers.len(), 2);
        assert_eq!(decoded.body_len, 42);
    }

    #[test]
    fn response_envelope_roundtrip() {
        let env = HttpResponseEnvelope {
            status: 200,
            headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
        };
        let json = serde_json::to_string(&env).unwrap();
        let decoded: HttpResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.status, 200);
        assert_eq!(decoded.headers.len(), 1);
    }

    #[test]
    fn extract_phantom_token_bearer() {
        let headers = vec![("Authorization".to_string(), "Bearer pt-abc".to_string())];
        let token = extract_phantom_token(&headers, "openai");
        assert_eq!(token.as_deref(), Some("pt-abc"));
    }

    #[test]
    fn extract_phantom_token_xapikey() {
        let headers = vec![("x-api-key".to_string(), "pt-def".to_string())];
        let token = extract_phantom_token(&headers, "anthropic");
        assert_eq!(token.as_deref(), Some("pt-def"));
    }

    #[test]
    fn extract_phantom_token_github() {
        let headers = vec![("Authorization".to_string(), "token pt-ghi".to_string())];
        let token = extract_phantom_token(&headers, "github");
        assert_eq!(token.as_deref(), Some("pt-ghi"));
    }

    #[test]
    fn extract_phantom_token_missing() {
        let headers = vec![("content-type".to_string(), "application/json".to_string())];
        let token = extract_phantom_token(&headers, "anthropic");
        assert!(token.is_none());
    }

    #[test]
    fn build_provider_meta_builtin() {
        let headers = vec![];
        let meta = build_provider_meta(&headers, "anthropic");
        assert_eq!(meta.env_var, "ANTHROPIC_API_KEY");
        assert_eq!(meta.header, "x-api-key");
        assert_eq!(meta.prefix, "");
    }

    #[test]
    fn build_provider_meta_unknown_provider() {
        let headers = vec![("x-custom-key".to_string(), "val".to_string())];
        let meta = build_provider_meta(&headers, "custom_provider");
        assert_eq!(meta.env_var, "CUSTOM_PROVIDER_API_KEY");
    }
}
