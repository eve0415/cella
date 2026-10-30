//! Phantom token generation and daemon registration for credential protection.

use std::collections::HashMap;
use std::path::Path;

use cella_config::settings::{Credentials, CustomCredentialProvider};
use cella_env::credential_providers::{self, CustomProviderInput, MergedProvider};
use cella_protocol::PhantomTokenEntry;
use tracing::{info, warn};

use crate::approved_providers::ApprovedProviders;

/// Generated phantom tokens ready for daemon registration and container injection.
pub struct PhantomTokenSet {
    pub entries: Vec<PhantomTokenEntry>,
    pub gh_phantom: Option<String>,
}

/// Generate phantom tokens for all detected credential providers.
pub fn generate_phantom_tokens(settings: &cella_config::CellaConfig) -> PhantomTokenSet {
    phantom_tokens_for(
        &settings.credentials,
        host_env,
        cella_env::gh_credential::gh_is_authenticated,
    )
}

/// Phantom tokens for every provider [`provider_selected`] picks, probing the
/// host through `env` and `gh_authenticated`.
fn phantom_tokens_for(
    credentials: &Credentials,
    env: impl Fn(&str) -> Option<String>,
    gh_authenticated: impl Fn() -> bool,
) -> PhantomTokenSet {
    let mut entries = Vec::new();
    let mut gh_phantom = None;

    for provider in merged_providers(credentials) {
        if !provider_selected(credentials, &provider, &env, &gh_authenticated) {
            continue;
        }
        let token = format!("pt-{}", uuid::Uuid::new_v4());
        if provider.id == "github" {
            gh_phantom = Some(token.clone());
        }
        entries.push(build_entry(&provider, token));
    }

    PhantomTokenSet {
        entries,
        gh_phantom,
    }
}

/// Built-in credential providers merged with the custom ones in `credentials`.
pub fn merged_providers(credentials: &Credentials) -> Vec<MergedProvider> {
    let custom_inputs: Vec<CustomProviderInput<'_>> = credentials
        .providers
        .iter()
        .map(|c| CustomProviderInput {
            name: &c.name,
            env: &c.env,
            domains: &c.domains,
            header: &c.header,
            prefix: &c.prefix,
        })
        .collect();
    credential_providers::merge_with_custom(&custom_inputs)
}

/// Whether `provider` gets a phantom token, and so a credential route.
///
/// GitHub is selected when gh forwarding is on and `gh_authenticated` reports
/// the host `gh` logged in; the probe runs only in that case. Every other
/// provider is selected when AI forwarding enables it and `env` returns a
/// non-empty value for its environment variable.
pub fn provider_selected(
    credentials: &Credentials,
    provider: &MergedProvider,
    env: impl Fn(&str) -> Option<String>,
    gh_authenticated: impl Fn() -> bool,
) -> bool {
    if provider.id == "github" {
        return credentials.gh && gh_authenticated();
    }
    credentials.ai.is_provider_enabled(&provider.id)
        && env(&provider.env_var).is_some_and(|v| !v.is_empty())
}

fn host_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
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

/// Register phantom tokens with the daemon via management socket.
/// Returns the per-container nonce on success.
pub async fn register_with_daemon(
    socket_path: &Path,
    container_name: &str,
    entries: &[PhantomTokenEntry],
) -> Option<String> {
    if entries.is_empty() {
        return None;
    }

    let req = cella_protocol::ManagementRequest::RegisterPhantomTokens {
        container_name: container_name.to_string(),
        tokens: entries.to_vec(),
    };

    match cella_daemon_client::send_management_request(socket_path, &req).await {
        Ok(cella_protocol::ManagementResponse::PhantomTokensRegistered {
            container_nonce, ..
        }) => {
            info!(
                "Registered {} phantom tokens for {container_name}",
                entries.len()
            );
            container_nonce
        }
        Ok(resp) => {
            warn!("Unexpected daemon response for phantom registration: {resp:?}");
            None
        }
        Err(e) => {
            warn!("Failed to register phantom tokens with daemon: {e}");
            None
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
    // The control file deliberately outlives the daemon so the port can be
    // reclaimed, so its contents are only worth trusting while a daemon is
    // listening. A graceful stop removes this socket; a daemon killed outright
    // leaves it behind, so this does not prove liveness on its own. Callers
    // reach here through `ensure_cella_daemon`, which does check the process
    // and clears a stale pair before anything reads this.
    if !data_dir.join("daemon.sock").exists() {
        return None;
    }
    let control_path = data_dir.join("daemon.control");
    let content = std::fs::read_to_string(&control_path).ok()?;
    let mut lines = content.lines();
    let port: u16 = lines.next()?.parse().ok()?;
    let token = lines.next()?.to_string();
    Some(cella_env::proxy::DaemonConnectionInfo {
        addr: format!("127.0.0.1:{port}"),
        token,
        container_name: container_name.to_string(),
        container_nonce: None,
    })
}

/// Generate phantom tokens and inject credential routes into the proxy config.
///
/// Returns the routes injected, which is none when no provider is selected,
/// the daemon is not reachable, or there is no proxy config to patch.
pub fn inject_routes_into_proxy_config(
    settings: &cella_config::CellaConfig,
    container_name: &str,
    env_fwd: &mut cella_env::EnvForwarding,
) -> Vec<cella_env::proxy::CredentialRouteConfig> {
    let phantom_set = generate_phantom_tokens(settings);
    if phantom_set.entries.is_empty() {
        return Vec::new();
    }
    let routes = build_credential_routes(&phantom_set.entries);
    let Some(daemon) = read_daemon_connection_info(container_name) else {
        return Vec::new();
    };
    if patch_proxy_config(env_fwd, &routes, &daemon) {
        routes
    } else {
        Vec::new()
    }
}

/// Inject credential routes into the proxy config file upload, returning
/// whether there was one to patch.
pub fn patch_proxy_config(
    env_fwd: &mut cella_env::EnvForwarding,
    routes: &[cella_env::proxy::CredentialRouteConfig],
    daemon: &cella_env::proxy::DaemonConnectionInfo,
) -> bool {
    let proxy_config_path = cella_env::PROXY_CONFIG_PATH;
    let Some(upload) = env_fwd
        .post_start
        .file_uploads
        .iter_mut()
        .find(|f| f.container_path == proxy_config_path)
    else {
        return false;
    };
    let base_json = String::from_utf8_lossy(&upload.content);
    let patched = cella_env::proxy::inject_credential_routes(&base_json, routes, daemon);
    upload.content = patched.into_bytes();
    true
}

/// Warnings for the rules a credential route keeps from applying.
///
/// The agent hands requests for a routed domain to the credential proxy before
/// it evaluates network rules, so a `respond` rule or an `allow` rule's
/// response headers never reach that domain. Block and plain allow rules are
/// not listed. A wildcard rule is listed for each routed domain it matches; it
/// still applies to the other domains it matches.
pub fn shadowed_rule_warnings(
    rules: &[cella_network::NetworkRule],
    routes: &[cella_env::proxy::CredentialRouteConfig],
) -> Vec<String> {
    let routed: std::collections::BTreeSet<&str> =
        routes.iter().map(|r| r.domain.as_str()).collect();
    rules
        .iter()
        .filter(|rule| {
            rule.action == cella_network::RuleAction::Respond || !rule.response_headers.is_empty()
        })
        .flat_map(|rule| {
            // Every rule left needs MITM, so this is the matcher's own domain
            // match against the rule's pattern.
            let matcher = cella_network::RuleMatcher::new(&cella_network::NetworkConfig {
                rules: vec![rule.clone()],
                ..Default::default()
            });
            routed
                .iter()
                .filter(move |domain| matcher.domain_needs_mitm(domain))
                .map(move |domain| {
                    format!(
                        "Network rule `{}: {}` does not apply to {domain}: credential protection routes that domain to the credential proxy, which takes precedence over network rules.",
                        rule.action.as_str(),
                        rule.label()
                    )
                })
        })
        .collect()
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

/// Partition custom providers into approved and unapproved groups.
///
/// Returns `(approved, needs_approval)`. Providers whose configuration
/// has changed since last approval appear in `needs_approval`.
pub fn filter_custom_providers<'a>(
    providers: &'a [CustomCredentialProvider],
    approved: &ApprovedProviders,
) -> (
    Vec<&'a CustomCredentialProvider>,
    Vec<&'a CustomCredentialProvider>,
) {
    let mut ok = Vec::new();
    let mut pending = Vec::new();

    for provider in providers {
        if approved.is_approved(provider) {
            ok.push(provider);
        } else {
            pending.push(provider);
        }
    }

    (ok, pending)
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

    fn test_provider(name: &str) -> CustomCredentialProvider {
        CustomCredentialProvider {
            name: name.to_string(),
            env: "KEY".to_string(),
            domains: vec!["api.example.com".to_string()],
            header: "Authorization".to_string(),
            prefix: "Bearer ".to_string(),
        }
    }

    fn env_of<'a>(vars: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    fn merged(credentials: &Credentials, id: &str) -> MergedProvider {
        merged_providers(credentials)
            .into_iter()
            .find(|p| p.id == id)
            .expect("provider is merged")
    }

    fn internal_credentials() -> Credentials {
        Credentials {
            providers: vec![CustomCredentialProvider {
                name: "internal".to_string(),
                env: "INTERNAL_KEY".to_string(),
                domains: vec!["api.internal.example".to_string()],
                header: "x-api-key".to_string(),
                prefix: String::new(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn provider_not_selected_without_env_var() {
        let credentials = Credentials::default();
        let anthropic = merged(&credentials, "anthropic");
        assert!(!provider_selected(
            &credentials,
            &anthropic,
            env_of(&[]),
            || false
        ));
        assert!(!provider_selected(
            &credentials,
            &anthropic,
            env_of(&[("ANTHROPIC_API_KEY", "")]),
            || false
        ));
        assert!(provider_selected(
            &credentials,
            &anthropic,
            env_of(&[("ANTHROPIC_API_KEY", "sk-test")]),
            || false
        ));
    }

    #[test]
    fn ai_providers_not_selected_when_ai_disabled() {
        let mut credentials = Credentials::default();
        credentials.ai.enabled = false;
        let anthropic = merged(&credentials, "anthropic");
        let env = env_of(&[("ANTHROPIC_API_KEY", "sk-test")]);
        assert!(!provider_selected(&credentials, &anthropic, env, || false));
    }

    #[test]
    fn disabled_provider_not_selected() {
        let mut credentials = Credentials::default();
        credentials
            .ai
            .providers
            .insert("anthropic".to_string(), false);
        let vars = [
            ("ANTHROPIC_API_KEY", "sk-test"),
            ("OPENAI_API_KEY", "sk-test"),
        ];
        let anthropic = merged(&credentials, "anthropic");
        let openai = merged(&credentials, "openai");
        assert!(!provider_selected(
            &credentials,
            &anthropic,
            env_of(&vars),
            || false
        ));
        assert!(provider_selected(
            &credentials,
            &openai,
            env_of(&vars),
            || false
        ));
    }

    #[test]
    fn custom_provider_selected_when_env_present() {
        let credentials = internal_credentials();
        let internal = merged(&credentials, "internal");
        assert!(provider_selected(
            &credentials,
            &internal,
            env_of(&[("INTERNAL_KEY", "secret")]),
            || false
        ));
        assert!(!provider_selected(
            &credentials,
            &internal,
            env_of(&[]),
            || false
        ));
    }

    #[test]
    fn github_selected_only_when_forwarded_and_authenticated() {
        let mut credentials = Credentials::default();
        let github = merged(&credentials, "github");
        assert!(provider_selected(
            &credentials,
            &github,
            env_of(&[]),
            || true
        ));
        assert!(!provider_selected(
            &credentials,
            &github,
            env_of(&[]),
            || false
        ));
        credentials.gh = false;
        assert!(!provider_selected(
            &credentials,
            &github,
            env_of(&[]),
            || { panic!("gh probed with gh forwarding off") }
        ));
    }

    /// Routes `phantom_tokens_for` decides with `env` and gh logged out.
    fn routes_with(
        credentials: &Credentials,
        vars: &[(&str, &str)],
    ) -> Vec<cella_env::proxy::CredentialRouteConfig> {
        build_credential_routes(&phantom_tokens_for(credentials, env_of(vars), || false).entries)
    }

    fn net_rule(domain: &str, action: cella_network::RuleAction) -> cella_network::NetworkRule {
        cella_network::NetworkRule {
            domain: domain.to_string(),
            paths: vec!["/v1/messages".to_string()],
            action,
            ..Default::default()
        }
    }

    fn header_rule(domain: &str) -> cella_network::NetworkRule {
        let mut rule = net_rule(domain, cella_network::RuleAction::Allow);
        rule.response_headers
            .insert("x-test-run".to_string(), "1".to_string());
        rule
    }

    const ANTHROPIC_KEY: [(&str, &str); 1] = [("ANTHROPIC_API_KEY", "sk-test")];

    #[test]
    fn shadowed_rule_warnings_name_rules_on_routed_domains() {
        let rules = [
            net_rule("api.anthropic.com", cella_network::RuleAction::Respond),
            header_rule("api.anthropic.com"),
        ];
        let routes = routes_with(&Credentials::default(), &ANTHROPIC_KEY);
        assert_eq!(
            shadowed_rule_warnings(&rules, &routes),
            [
                "Network rule `respond: api.anthropic.com [/v1/messages]` does not apply to api.anthropic.com: credential protection routes that domain to the credential proxy, which takes precedence over network rules.",
                "Network rule `allow: api.anthropic.com [/v1/messages]` does not apply to api.anthropic.com: credential protection routes that domain to the credential proxy, which takes precedence over network rules.",
            ]
        );
    }

    #[test]
    fn shadowed_rule_warnings_need_a_route() {
        let rules = [net_rule(
            "api.anthropic.com",
            cella_network::RuleAction::Respond,
        )];
        let routes = routes_with(&Credentials::default(), &[]);
        assert!(shadowed_rule_warnings(&rules, &routes).is_empty());
    }

    #[test]
    fn shadowed_rule_warnings_skip_rules_routing_does_not_shadow() {
        let rules = [
            net_rule("api.anthropic.com", cella_network::RuleAction::Block),
            net_rule("api.anthropic.com", cella_network::RuleAction::Allow),
            net_rule("api.openai.com", cella_network::RuleAction::Respond),
        ];
        let routes = routes_with(&Credentials::default(), &ANTHROPIC_KEY);
        assert!(shadowed_rule_warnings(&rules, &routes).is_empty());
    }

    #[test]
    fn shadowed_rule_warnings_list_wildcard_rules_with_the_routed_domain() {
        let rules = [net_rule(
            "*.anthropic.com",
            cella_network::RuleAction::Respond,
        )];
        let routes = routes_with(&Credentials::default(), &ANTHROPIC_KEY);
        let warnings = shadowed_rule_warnings(&rules, &routes);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].starts_with(
                "Network rule `respond: *.anthropic.com [/v1/messages]` does not apply to api.anthropic.com:"
            ),
            "{warnings:?}"
        );
    }

    #[test]
    fn filter_separates_approved_from_pending() {
        let mut store = ApprovedProviders::load(Path::new("/nonexistent"));
        let a = test_provider("approved-one");
        let b = test_provider("pending-one");
        store.approve(&a);

        let providers = vec![a, b];
        let (ok, pending) = filter_custom_providers(&providers, &store);
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].name, "approved-one");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].name, "pending-one");
    }

    #[test]
    fn filter_empty_providers_returns_empty() {
        let store = ApprovedProviders::load(Path::new("/nonexistent"));
        let (ok, pending) = filter_custom_providers(&[], &store);
        assert!(ok.is_empty());
        assert!(pending.is_empty());
    }

    #[test]
    fn filter_changed_provider_needs_reapproval() {
        let mut store = ApprovedProviders::load(Path::new("/nonexistent"));
        let original = test_provider("evolving");
        store.approve(&original);

        let mut changed = original;
        changed.domains = vec!["evil.example.com".to_string()];
        let providers = vec![changed];

        let (ok, pending) = filter_custom_providers(&providers, &store);
        assert!(ok.is_empty());
        assert_eq!(pending.len(), 1);
    }
}
