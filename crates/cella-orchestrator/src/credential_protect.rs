//! Phantom token generation and daemon registration for credential protection.

use std::collections::HashMap;
use std::path::Path;

use cella_env::credential_providers::{self, CustomProviderInput, MergedProvider};
use cella_protocol::PhantomTokenEntry;
use tracing::{info, warn};

/// Generated phantom tokens ready for daemon registration and container injection.
pub struct PhantomTokenSet {
    pub entries: Vec<PhantomTokenEntry>,
    pub gh_phantom: Option<String>,
}

/// Generate phantom tokens for all detected credential providers.
pub fn generate_phantom_tokens(settings: &cella_config::CellaConfig) -> PhantomTokenSet {
    let custom_inputs: Vec<CustomProviderInput<'_>> = settings
        .credentials
        .providers
        .iter()
        .map(|c| CustomProviderInput {
            name: &c.name,
            env: &c.env,
            domain: &c.domain,
            header: &c.header,
            prefix: &c.prefix,
        })
        .collect();

    let providers = credential_providers::merge_with_custom(&custom_inputs);

    let mut entries = Vec::new();
    let mut gh_phantom = None;

    let ai = &settings.credentials.ai;

    for provider in &providers {
        if provider.id == "github" {
            if settings.credentials.gh && cella_env::gh_credential::gh_is_authenticated() {
                let token = format!("pt-{}", uuid::Uuid::new_v4());
                gh_phantom = Some(token.clone());
                entries.push(build_entry(provider, token));
            }
            continue;
        }

        if !ai.enabled || !ai.is_provider_enabled(&provider.id) {
            continue;
        }

        if has_env_var(&provider.env_var) {
            let token = format!("pt-{}", uuid::Uuid::new_v4());
            entries.push(build_entry(provider, token));
        }
    }

    PhantomTokenSet {
        entries,
        gh_phantom,
    }
}

fn build_entry(provider: &MergedProvider, phantom_token: String) -> PhantomTokenEntry {
    PhantomTokenEntry {
        provider_id: provider.id.clone(),
        phantom_token,
        env_var: provider.env_var.clone(),
        domains: provider.domains.clone(),
        header: provider.header.clone(),
        prefix: provider.prefix.clone(),
    }
}

fn has_env_var(name: &str) -> bool {
    std::env::var(name).ok().is_some_and(|v| !v.is_empty())
}

/// Register phantom tokens with the daemon via management socket.
pub async fn register_with_daemon(
    socket_path: &Path,
    container_name: &str,
    entries: &[PhantomTokenEntry],
) -> bool {
    if entries.is_empty() {
        return true;
    }

    let req = cella_protocol::ManagementRequest::RegisterPhantomTokens {
        container_name: container_name.to_string(),
        tokens: entries.to_vec(),
    };

    match cella_daemon_client::send_management_request(socket_path, &req).await {
        Ok(cella_protocol::ManagementResponse::PhantomTokensRegistered { .. }) => {
            info!(
                "Registered {} phantom tokens for {container_name}",
                entries.len()
            );
            true
        }
        Ok(resp) => {
            warn!("Unexpected daemon response for phantom registration: {resp:?}");
            false
        }
        Err(e) => {
            warn!("Failed to register phantom tokens with daemon: {e}");
            false
        }
    }
}

/// Build credential route configs from phantom token entries.
///
/// Generates one route per domain per provider, so multi-domain providers
/// (e.g. GitHub with `github.com` + `api.github.com`) get separate routes.
pub fn build_credential_routes(
    entries: &[PhantomTokenEntry],
) -> Vec<cella_env::proxy::CredentialRouteConfig> {
    entries
        .iter()
        .flat_map(|e| {
            e.domains
                .iter()
                .map(move |domain| cella_env::proxy::CredentialRouteConfig {
                    domain: domain.clone(),
                    provider_id: e.provider_id.clone(),
                })
        })
        .collect()
}

/// Read daemon control connection info (addr + token) for credential proxying.
pub fn read_daemon_connection_info(
    container_name: &str,
) -> Option<cella_env::proxy::DaemonConnectionInfo> {
    let data_dir = cella_env::paths::cella_data_dir()?;
    let control_path = data_dir.join("daemon.control");
    let content = std::fs::read_to_string(&control_path).ok()?;
    let mut lines = content.lines();
    let port: u16 = lines.next()?.parse().ok()?;
    let token = lines.next()?.to_string();
    Some(cella_env::proxy::DaemonConnectionInfo {
        addr: format!("127.0.0.1:{port}"),
        token,
        container_name: container_name.to_string(),
    })
}

/// Generate phantom tokens and inject credential routes into the proxy config.
pub fn inject_routes_into_proxy_config(
    settings: &cella_config::CellaConfig,
    container_name: &str,
    env_fwd: &mut cella_env::EnvForwarding,
) {
    let phantom_set = generate_phantom_tokens(settings);
    if phantom_set.entries.is_empty() {
        return;
    }
    let routes = build_credential_routes(&phantom_set.entries);
    let Some(daemon) = read_daemon_connection_info(container_name) else {
        return;
    };
    patch_proxy_config(env_fwd, &routes, &daemon);
}

/// Inject credential routes into the proxy config file upload.
pub fn patch_proxy_config(
    env_fwd: &mut cella_env::EnvForwarding,
    routes: &[cella_env::proxy::CredentialRouteConfig],
    daemon: &cella_env::proxy::DaemonConnectionInfo,
) {
    let proxy_config_path = cella_env::PROXY_CONFIG_PATH;
    if let Some(upload) = env_fwd
        .post_start
        .file_uploads
        .iter_mut()
        .find(|f| f.container_path == proxy_config_path)
    {
        let base_json = String::from_utf8_lossy(&upload.content);
        let patched = cella_env::proxy::inject_credential_routes(&base_json, routes, daemon);
        upload.content = patched.into_bytes();
    }
}

/// Add the credential protection label to container labels.
pub fn add_protect_label<S: std::hash::BuildHasher>(
    labels: &mut HashMap<String, String, S>,
    container_name: &str,
) {
    labels.insert(
        "dev.cella.credential_protect".to_string(),
        "true".to_string(),
    );
    labels.insert(
        "dev.cella.container_name".to_string(),
        container_name.to_string(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_entry_includes_all_fields() {
        let provider = MergedProvider {
            id: "anthropic".to_string(),
            env_var: "ANTHROPIC_API_KEY".to_string(),
            domains: vec!["api.anthropic.com".to_string()],
            header: "x-api-key".to_string(),
            prefix: String::new(),
        };
        let entry = build_entry(&provider, "pt-test".to_string());
        assert_eq!(entry.provider_id, "anthropic");
        assert_eq!(entry.phantom_token, "pt-test");
        assert_eq!(entry.header, "x-api-key");
        assert_eq!(entry.prefix, "");
    }

    #[test]
    fn build_credential_routes_from_entries() {
        let entries = vec![PhantomTokenEntry {
            provider_id: "anthropic".to_string(),
            phantom_token: "pt-abc".to_string(),
            env_var: "ANTHROPIC_API_KEY".to_string(),
            domains: vec!["api.anthropic.com".to_string()],
            header: "x-api-key".to_string(),
            prefix: String::new(),
        }];
        let routes = build_credential_routes(&entries);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].domain, "api.anthropic.com");
        assert_eq!(routes[0].provider_id, "anthropic");
    }

    #[test]
    fn build_credential_routes_expands_multi_domain() {
        let entries = vec![PhantomTokenEntry {
            provider_id: "github".to_string(),
            phantom_token: "pt-gh".to_string(),
            env_var: "GH_TOKEN".to_string(),
            domains: vec!["github.com".to_string(), "api.github.com".to_string()],
            header: "Authorization".to_string(),
            prefix: "token ".to_string(),
        }];
        let routes = build_credential_routes(&entries);
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].domain, "github.com");
        assert_eq!(routes[1].domain, "api.github.com");
        assert!(routes.iter().all(|r| r.provider_id == "github"));
    }

    #[test]
    fn add_protect_label_sets_both() {
        let mut labels = HashMap::new();
        add_protect_label(&mut labels, "cella-test-main");
        assert_eq!(labels["dev.cella.credential_protect"], "true");
        assert_eq!(labels["dev.cella.container_name"], "cella-test-main");
    }
}
