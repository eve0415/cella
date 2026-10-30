//! `cella network` subcommands for inspecting proxy and blocking state.

use std::path::PathBuf;

use cella_orchestrator::credential_protect;
use clap::{Args, Subcommand};

use crate::picker;

#[derive(Args)]
pub struct NetworkArgs {
    #[command(subcommand)]
    command: NetworkCommand,
}

#[derive(Subcommand)]
enum NetworkCommand {
    /// Show network proxy and blocking status.
    Status,
    /// Test whether a URL would be blocked or allowed.
    Test {
        /// The URL to test (e.g., `https://api.prod.internal/v1/data`).
        url: String,
    },
    /// View the proxy log from a running container.
    Log {
        /// Follow log output (streams new entries in real-time).
        #[arg(short, long)]
        follow: bool,

        /// Number of lines to show from the end of the log.
        #[arg(long, default_value = "100")]
        tail: u32,

        /// Explicit workspace folder path (defaults to current directory).
        #[arg(long)]
        workspace_folder: Option<PathBuf>,

        #[command(flatten)]
        backend: crate::backend::BackendArgs,
    },
}

impl NetworkArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self.command {
            NetworkCommand::Status => execute_status(),
            NetworkCommand::Test { url } => execute_test(&url),
            NetworkCommand::Log {
                follow,
                tail,
                workspace_folder,
                backend,
            } => execute_log(follow, tail, workspace_folder.as_deref(), &backend).await,
        }
    }
}

fn execute_status() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let workspace_root = std::env::current_dir()?;
    let resolved = cella_config::devcontainer::resolve::config(&workspace_root, None).ok();
    let settings = cella_config::CellaConfig::load(&workspace_root, resolved.as_ref())?;
    let net_config = settings
        .network
        .to_network_config(cella_network::NetworkRulePolicy::Enforce)?;

    if !net_config.proxy.enabled {
        println!("Proxy: disabled");
        return Ok(());
    }

    let proxy_vars = cella_network::proxy_env::ProxyEnvVars::detect(&net_config.proxy);
    let has_proxy = proxy_vars
        .as_ref()
        .is_some_and(cella_network::ProxyEnvVars::has_proxy);

    if net_config.has_rules() {
        println!("Proxy: active (localhost:{})", net_config.proxy.proxy_port);
    } else if has_proxy {
        println!("Proxy: forwarding (passthrough)");
    } else {
        println!("Proxy: inactive (no proxy detected, no rules)");
    }

    if let Some(ref vars) = proxy_vars {
        if let Some(ref http) = vars.http_proxy {
            println!("Upstream HTTP: {http}");
        }
        if let Some(ref https) = vars.https_proxy {
            println!("Upstream HTTPS: {https}");
        }
    }

    // CA status.
    let ca_dir = dirs_home().join(".cella/proxy/ca.pem");
    if ca_dir.exists() {
        println!("CA: auto-generated (~/.cella/proxy/ca.pem)");
    } else {
        println!("CA: not generated");
    }

    // Rules.
    let mode_str = match net_config.mode {
        cella_network::NetworkMode::Denylist => "denylist",
        cella_network::NetworkMode::Allowlist => "allowlist",
    };
    println!("Mode: {mode_str} ({} rules)", net_config.rules.len());

    for rule in &net_config.rules {
        println!("{}", format_rule_line(rule));
    }

    Ok(())
}

fn execute_test(url: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let workspace_root = std::env::current_dir()?;
    let resolved = cella_config::devcontainer::resolve::config(&workspace_root, None).ok();
    let settings = cella_config::CellaConfig::load(&workspace_root, resolved.as_ref())?;
    let net_config = settings
        .network
        .to_network_config(cella_network::NetworkRulePolicy::Enforce)?;

    for line in test_report(net_config, url, |domain| {
        is_credential_routed(
            &settings.credentials,
            domain,
            |name| std::env::var(name).ok(),
            cella_env::gh_credential::gh_is_authenticated,
        )
    }) {
        println!("{line}");
    }

    Ok(())
}

async fn execute_log(
    follow: bool,
    tail: u32,
    workspace_folder: Option<&std::path::Path>,
    backend: &crate::backend::BackendArgs,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = backend.resolve_client().await?;
    let cwd = super::resolve_workspace_folder(workspace_folder)?;

    let container = match client.as_ref().find_container(&cwd).await? {
        Some(c) => c,
        None if workspace_folder.is_none() => {
            let containers = client.as_ref().list_cella_containers(false).await?;
            picker::resolve_container_interactive(
                &containers,
                None,
                "Select a container for proxy log:",
                None,
            )?
        }
        None => return Err("No cella container found for this workspace".into()),
    };

    let cmd = if follow {
        vec![
            "tail".to_string(),
            "-f".to_string(),
            "-n".to_string(),
            tail.to_string(),
            "/tmp/.cella/proxy.log".to_string(),
        ]
    } else {
        vec![
            "tail".to_string(),
            "-n".to_string(),
            tail.to_string(),
            "/tmp/.cella/proxy.log".to_string(),
        ]
    };

    let opts = cella_backend::ExecOptions {
        cmd,
        user: None,
        env: None,
        working_dir: None,
    };

    if follow {
        client
            .as_ref()
            .exec_stream(
                &container.id,
                &opts,
                Box::new(std::io::stdout()),
                Box::new(std::io::stderr()),
            )
            .await?;
    } else {
        let result = client.as_ref().exec_command(&container.id, &opts).await?;
        if !result.stdout.is_empty() {
            print!("{}", result.stdout);
        }
        if !result.stderr.is_empty() {
            eprint!("{}", result.stderr);
        }
    }

    Ok(())
}

/// One `cella network status` line for a rule.
fn format_rule_line(rule: &cella_network::NetworkRule) -> String {
    let line = format!("  {}: {}", rule.action.as_str(), rule.label());

    match rule.action {
        cella_network::RuleAction::Respond if !rule.responses.is_empty() => {
            let statuses = rule.responses.iter().map(|r| r.status);
            format!("{line} ({})", format_sequence(statuses, " \u{2192} "))
        }
        cella_network::RuleAction::Allow if !rule.response_headers.is_empty() => {
            format!("{line} (+{} response headers)", rule.response_headers.len())
        }
        _ => line,
    }
}

/// Statuses of a response sequence in serving order, joined by `separator`.
fn format_sequence(statuses: impl IntoIterator<Item = u16>, separator: &str) -> String {
    statuses
        .into_iter()
        .map(|status| status.to_string())
        .collect::<Vec<_>>()
        .join(separator)
}

/// Whether requests to `domain` go to the credential proxy, which bypasses
/// network rules: protection is on and a provider serving the domain gets a
/// phantom token. `env` and `gh_authenticated` probe the host as
/// [`credential_protect::provider_selected`] describes.
fn is_credential_routed(
    credentials: &cella_config::settings::Credentials,
    domain: &str,
    env: impl Fn(&str) -> Option<String>,
    gh_authenticated: impl Fn() -> bool,
) -> bool {
    credentials.protect
        && credential_protect::merged_providers(credentials)
            .iter()
            .filter(|p| p.domains.iter().any(|d| d == domain))
            .any(|p| credential_protect::provider_selected(credentials, p, &env, &gh_authenticated))
}

/// The `cella network test` output lines for `url` under `net_config`.
///
/// Evaluates through the matcher even with no rules, since the mode decides
/// the outcome then: a denylist allows everything, an allowlist blocks it.
fn test_report(
    net_config: cella_network::NetworkConfig,
    url: &str,
    credential_routed: impl Fn(&str) -> bool,
) -> Vec<String> {
    let (host, path) = match parse_test_url(url) {
        Ok(target) => target,
        Err(reason) => {
            return vec![
                format!("\u{2717} REJECTED: {url}"),
                format!("  the proxy answers 400 Bad Request: {reason}"),
            ];
        }
    };
    let (matcher, payloads) = cella_network::RuleMatcher::with_payloads(net_config);
    let verdict = matcher.evaluate(&host, &path);
    format_test_outcome(url, &verdict, &payloads, credential_routed(&host))
}

/// The host and path the agent proxy evaluates rules against for `url`, or
/// why the proxy would reject the request with 400.
///
/// The URL is parsed with the request-target grammar hyper applies in the
/// agent, so a URL it refuses is rejected here too. An `http://` URL is an
/// absolute-form plain target. An `https://` URL is a CONNECT target, which
/// MITM interception evaluates with the same host; clients drop its userinfo
/// before sending CONNECT. A URL without a scheme is the CONNECT target
/// itself, so userinfo there is rejected as the agent rejects it. The host is
/// the canonical one the agent matches; the path has no query string or
/// fragment.
fn parse_test_url(url: &str) -> Result<(String, String), String> {
    let scheme = url
        .split_once("://")
        .map(|(scheme, _)| scheme)
        .filter(|scheme| {
            scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        });
    let uri: http::Uri = match scheme {
        Some(_) => url.parse(),
        None => format!("https://{url}").parse(),
    }
    .map_err(|e: http::uri::InvalidUri| e.to_string())?;
    let authority = uri.authority().map_or("", http::uri::Authority::as_str);
    let target = match scheme {
        None => cella_network::authority::parse_connect_target(authority),
        Some(s) if s.eq_ignore_ascii_case("http") => {
            cella_network::authority::parse_plain_authority(authority)
        }
        Some(s) if s.eq_ignore_ascii_case("https") => {
            let host_and_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
            cella_network::authority::parse_connect_target(host_and_port)
        }
        Some(s) => {
            return Err(format!(
                "unsupported scheme `{s}` (only http and https go through the proxy)"
            ));
        }
    };
    let host = target.map_err(|e| e.to_string())?.host;
    let path = uri.path();
    Ok((host, if path.is_empty() { "/" } else { path }.to_string()))
}

/// The `cella network test` output lines for a verdict, reading a `respond`
/// rule's statuses from the `payloads` its matcher moved out of the config.
fn format_test_outcome(
    url: &str,
    verdict: &cella_network::rules::RuleVerdict,
    payloads: &cella_network::rules::RulePayloads,
    credential_routed: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    match &verdict.action {
        cella_network::rules::VerdictAction::Allow { .. } => {
            lines.push(format!("\u{2713} ALLOWED: {url}"));
            lines.push(format!("  {}", verdict.reason));
        }
        cella_network::rules::VerdictAction::Block => {
            lines.push(format!("\u{2717} BLOCKED: {url}"));
            lines.push(format!("  {}", verdict.reason));
        }
        cella_network::rules::VerdictAction::Respond(rule) => {
            lines.push(format!("\u{21a9} RESPONDS: {url}"));
            lines.push(format!("  {}", verdict.reason));
            let responses = payloads.responses.get(rule.index().get());
            let statuses = responses.into_iter().flatten().map(|r| r.status);
            lines.push(format!("  sequence: {}", format_sequence(statuses, ", ")));
        }
    }
    if credential_routed {
        lines.push(
            "  note: credential-protected domain; requests go to the credential proxy and rules are not applied"
                .to_string(),
        );
    }
    lines
}

fn dirs_home() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(url: &str) -> (String, String) {
        parse_test_url(url).unwrap()
    }

    fn target(host: &str, path: &str) -> (String, String) {
        (host.to_string(), path.to_string())
    }

    #[test]
    fn parse_test_url_splits_host_and_path() {
        for (url, host, path) in [
            (
                "https://api.prod.internal/v1/data",
                "api.prod.internal",
                "/v1/data",
            ),
            ("http://example.com:8080/api", "example.com", "/api"),
            ("https://example.com/", "example.com", "/"),
            ("http://example.com", "example.com", "/"),
            (
                "https://api.example.com/v2/users/123",
                "api.example.com",
                "/v2/users/123",
            ),
            (
                "https://api.staging.example.com:9443/health",
                "api.staging.example.com",
                "/health",
            ),
            ("http://localhost:3000/api", "localhost", "/api"),
            ("http://192.168.1.1:8080/status", "192.168.1.1", "/status"),
            ("HTTPS://Example.COM/Path", "example.com", "/Path"),
        ] {
            assert_eq!(parsed(url), target(host, path), "{url}");
        }
    }

    #[test]
    fn parse_test_url_treats_missing_scheme_as_https() {
        assert_eq!(parsed("example.com"), target("example.com", "/"));
        assert_eq!(parsed("example.com:8080"), target("example.com", "/"));
        assert_eq!(
            parsed("api.internal/v1/resource"),
            target("api.internal", "/v1/resource")
        );
        assert_eq!(
            parsed("example.com/go?to=http://other.example"),
            target("example.com", "/go")
        );
    }

    #[test]
    fn parse_test_url_drops_query_and_fragment() {
        assert_eq!(
            parsed("https://example.com/search?q=test"),
            target("example.com", "/search")
        );
        assert_eq!(
            parsed("https://example.com/page#section"),
            target("example.com", "/page")
        );
        assert_eq!(parsed("http://example.com?q=1"), target("example.com", "/"));
        assert_eq!(
            parsed("https://example.com#top"),
            target("example.com", "/")
        );
    }

    #[test]
    fn parse_test_url_canonicalises_ipv6_hosts() {
        assert_eq!(parsed("http://[::1]:8080/x"), target("::1", "/x"));
        assert_eq!(parsed("http://[::1]/x"), target("::1", "/x"));
        assert_eq!(parsed("https://[0:0::1]:8443/"), target("::1", "/"));
        assert_eq!(
            parsed("http://[::ffff:127.0.0.1]/"),
            target("127.0.0.1", "/")
        );
    }

    #[test]
    fn parse_test_url_handles_userinfo_like_the_agent() {
        assert_eq!(
            parsed("http://user:pw@example.com/a"),
            target("example.com", "/a")
        );
        assert_eq!(
            parsed("https://user:pw@example.com/a"),
            target("example.com", "/a")
        );
        assert!(parse_test_url("user@example.com:443").is_err());
    }

    fn report(rules: Vec<cella_network::NetworkRule>, url: &str) -> Vec<String> {
        test_report(
            cella_network::NetworkConfig {
                rules,
                ..Default::default()
            },
            url,
            |_| false,
        )
    }

    fn first_line(rules: Vec<cella_network::NetworkRule>, url: &str) -> String {
        report(rules, url).swap_remove(0)
    }

    #[test]
    fn test_report_rejects_targets_the_request_parser_refuses() {
        let given_verdicts: Vec<String> = [
            "http://evil.example\\@allowed.example/x",
            "http://%61llowed.example/",
            "http://allowed.example{x}/",
        ]
        .into_iter()
        .map(|url| first_line(vec![], url))
        .filter(|line| !line.starts_with("\u{2717} REJECTED: "))
        .collect();
        assert_eq!(given_verdicts, Vec::<String>::new());
    }

    #[test]
    fn test_report_evaluates_https_host_without_userinfo() {
        let block = cella_network::NetworkRule {
            domain: "blocked.example".to_string(),
            action: cella_network::RuleAction::Block,
            ..Default::default()
        };
        let url = "https://u:p@blocked.example/";
        assert_eq!(
            first_line(vec![block], url),
            format!("\u{2717} BLOCKED: {url}")
        );
    }

    #[test]
    fn test_report_rejects_userinfo_in_bare_connect_target() {
        let url = "u@host:443";
        assert_eq!(first_line(vec![], url), format!("\u{2717} REJECTED: {url}"));
    }

    #[test]
    fn parse_test_url_rejects_other_schemes() {
        assert!(parse_test_url("ftp://example.com/").is_err());
        assert!(parse_test_url("ws://example.com/").is_err());
    }

    #[test]
    fn dirs_home_returns_path() {
        let home = dirs_home();
        assert!(!home.as_os_str().is_empty());
    }

    fn response(status: u16) -> cella_network::ResponseSpec {
        cella_network::ResponseSpec {
            status,
            headers: std::collections::BTreeMap::new(),
            body: String::new(),
        }
    }

    fn rule(action: cella_network::RuleAction, paths: &[&str]) -> cella_network::NetworkRule {
        cella_network::NetworkRule {
            domain: "api.anthropic.com".to_string(),
            paths: paths.iter().map(ToString::to_string).collect(),
            action,
            ..Default::default()
        }
    }

    /// The verdict for `path` on api.anthropic.com, and the payloads of the
    /// matcher that gave it.
    fn verdict_for(
        rules: Vec<cella_network::NetworkRule>,
        path: &str,
    ) -> (
        cella_network::rules::RuleVerdict,
        cella_network::rules::RulePayloads,
    ) {
        let (matcher, payloads) =
            cella_network::RuleMatcher::with_payloads(cella_network::NetworkConfig {
                rules,
                ..Default::default()
            });
        (matcher.evaluate("api.anthropic.com", path), payloads)
    }

    fn respond_rule() -> cella_network::NetworkRule {
        cella_network::NetworkRule {
            responses: vec![response(529), response(200)],
            ..rule(cella_network::RuleAction::Respond, &["/v1/messages"])
        }
    }

    const CREDENTIAL_NOTE: &str = "  note: credential-protected domain; requests go to the credential proxy and rules are not applied";

    #[test]
    fn status_line_for_respond_rule_shows_sequence() {
        assert_eq!(
            format_rule_line(&respond_rule()),
            "  respond: api.anthropic.com [/v1/messages] (529 \u{2192} 200)"
        );
    }

    #[test]
    fn status_line_for_respond_rule_without_paths() {
        let mut r = respond_rule();
        r.paths.clear();
        r.responses = vec![response(503)];
        assert_eq!(format_rule_line(&r), "  respond: api.anthropic.com (503)");
    }

    #[test]
    fn status_line_for_allow_rule_counts_response_headers() {
        let mut r = rule(cella_network::RuleAction::Allow, &[]);
        r.response_headers
            .insert("x-test-run".to_string(), "1".to_string());
        r.response_headers
            .insert("x-other".to_string(), "2".to_string());
        assert_eq!(
            format_rule_line(&r),
            "  allow: api.anthropic.com (+2 response headers)"
        );
    }

    #[test]
    fn status_line_for_plain_rules_has_no_suffix() {
        assert_eq!(
            format_rule_line(&rule(cella_network::RuleAction::Block, &["/a", "/b"])),
            "  block: api.anthropic.com [/a, /b]"
        );
        assert_eq!(
            format_rule_line(&rule(cella_network::RuleAction::Allow, &[])),
            "  allow: api.anthropic.com"
        );
    }

    #[test]
    fn test_outcome_for_respond_lists_sequence() {
        let (verdict, payloads) = verdict_for(vec![respond_rule()], "/v1/messages");
        let lines = format_test_outcome(
            "https://api.anthropic.com/v1/messages",
            &verdict,
            &payloads,
            false,
        );
        assert_eq!(
            lines,
            [
                "\u{21a9} RESPONDS: https://api.anthropic.com/v1/messages".to_string(),
                format!("  {}", verdict.reason),
                "  sequence: 529, 200".to_string(),
            ]
        );
    }

    #[test]
    fn test_outcome_lists_the_matched_rules_own_sequence() {
        let other = cella_network::NetworkRule {
            domain: "other.example".to_string(),
            responses: vec![response(418)],
            ..rule(cella_network::RuleAction::Respond, &[])
        };
        let (verdict, payloads) = verdict_for(vec![other, respond_rule()], "/v1/messages");
        let lines = format_test_outcome("https://api.anthropic.com/", &verdict, &payloads, false);
        assert_eq!(
            lines.get(2).map(String::as_str),
            Some("  sequence: 529, 200")
        );
    }

    #[test]
    fn test_outcome_adds_credential_note_when_routed() {
        let (verdict, payloads) = verdict_for(vec![respond_rule()], "/v1/messages");
        let url = "https://api.anthropic.com/v1/messages";
        let lines = format_test_outcome(url, &verdict, &payloads, true);
        assert_eq!(lines.last().map(String::as_str), Some(CREDENTIAL_NOTE));
        let without = format_test_outcome(url, &verdict, &payloads, false);
        assert!(!without.iter().any(|l| l.contains("note:")));
    }

    #[test]
    fn test_outcome_for_allowed_and_blocked() {
        let (allowed, payloads) = verdict_for(vec![], "/");
        assert_eq!(
            format_test_outcome("https://api.anthropic.com/", &allowed, &payloads, false),
            [
                "\u{2713} ALLOWED: https://api.anthropic.com/".to_string(),
                format!("  {}", allowed.reason),
            ]
        );
        let (blocked, payloads) =
            verdict_for(vec![rule(cella_network::RuleAction::Block, &[])], "/");
        assert_eq!(
            format_test_outcome("https://api.anthropic.com/", &blocked, &payloads, false),
            [
                "\u{2717} BLOCKED: https://api.anthropic.com/".to_string(),
                format!("  {}", blocked.reason),
            ]
        );
    }

    fn report_for(mode: cella_network::NetworkMode, credential_routed: bool) -> Vec<String> {
        test_report(
            cella_network::NetworkConfig {
                mode,
                ..Default::default()
            },
            "https://api.anthropic.com/v1/messages?x=1",
            |_| credential_routed,
        )
    }

    #[test]
    fn test_report_blocks_everything_in_allowlist_mode_without_rules() {
        let lines = report_for(cella_network::NetworkMode::Allowlist, false);
        assert_eq!(
            lines,
            [
                "\u{2717} BLOCKED: https://api.anthropic.com/v1/messages?x=1".to_string(),
                "  blocked (no matching allow rule)".to_string(),
            ]
        );
    }

    #[test]
    fn test_report_allows_everything_in_denylist_mode_without_rules() {
        let lines = report_for(cella_network::NetworkMode::Denylist, false);
        assert_eq!(
            lines,
            [
                "\u{2713} ALLOWED: https://api.anthropic.com/v1/messages?x=1".to_string(),
                "  allowed (no matching deny rule)".to_string(),
            ]
        );
    }

    #[test]
    fn test_report_keeps_credential_note_without_rules() {
        let lines = report_for(cella_network::NetworkMode::Allowlist, true);
        assert_eq!(lines.last().map(String::as_str), Some(CREDENTIAL_NOTE));
    }

    fn env_of<'a>(vars: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    const ANTHROPIC_KEY: &[(&str, &str)] = &[("ANTHROPIC_API_KEY", "sk-test")];

    #[test]
    fn credential_routing_requires_protection_and_provider_domain() {
        let mut credentials = cella_config::settings::Credentials::default();
        let routed = |c: &cella_config::settings::Credentials, domain: &str| {
            is_credential_routed(c, domain, env_of(ANTHROPIC_KEY), || false)
        };
        assert!(!routed(&credentials, "api.anthropic.com"));
        credentials.protect = true;
        assert!(routed(&credentials, "api.anthropic.com"));
        assert!(!routed(&credentials, "example.com"));
    }

    #[test]
    fn credential_routing_requires_a_phantom_token() {
        let mut credentials = cella_config::settings::Credentials {
            protect: true,
            ..Default::default()
        };
        assert!(!is_credential_routed(
            &credentials,
            "api.anthropic.com",
            env_of(&[]),
            || false
        ));
        credentials.ai.enabled = false;
        assert!(!is_credential_routed(
            &credentials,
            "api.anthropic.com",
            env_of(ANTHROPIC_KEY),
            || false
        ));
        credentials.ai.enabled = true;
        credentials
            .ai
            .providers
            .insert("anthropic".to_string(), false);
        assert!(!is_credential_routed(
            &credentials,
            "api.anthropic.com",
            env_of(ANTHROPIC_KEY),
            || false
        ));
    }

    #[test]
    fn credential_routing_includes_custom_provider_domains() {
        let credentials = cella_config::settings::Credentials {
            protect: true,
            providers: vec![cella_config::settings::CustomCredentialProvider {
                name: "internal".to_string(),
                env: "INTERNAL_KEY".to_string(),
                domains: vec!["api.internal.example".to_string()],
                header: "x-api-key".to_string(),
                prefix: String::new(),
            }],
            ..Default::default()
        };
        let domain = "api.internal.example";
        let key = [("INTERNAL_KEY", "secret")];
        assert!(is_credential_routed(
            &credentials,
            domain,
            env_of(&key),
            || false
        ));
        assert!(!is_credential_routed(
            &credentials,
            domain,
            env_of(&[]),
            || false
        ));
    }

    #[test]
    fn credential_routing_for_github_needs_authenticated_gh() {
        let credentials = cella_config::settings::Credentials {
            protect: true,
            ..Default::default()
        };
        assert!(is_credential_routed(
            &credentials,
            "api.github.com",
            env_of(&[]),
            || true
        ));
        assert!(!is_credential_routed(
            &credentials,
            "api.github.com",
            env_of(&[]),
            || false
        ));
    }

    #[test]
    fn stripped_query_matches_path_rule() {
        let lines = test_report(
            cella_network::NetworkConfig {
                rules: vec![respond_rule()],
                ..Default::default()
            },
            "https://api.anthropic.com/v1/messages?beta=true#x",
            |_| false,
        );
        assert_eq!(
            lines.first().map(String::as_str),
            Some("\u{21a9} RESPONDS: https://api.anthropic.com/v1/messages?beta=true#x")
        );
    }

    fn denylist_report(domain: &str, url: &str) -> Vec<String> {
        test_report(
            cella_network::NetworkConfig {
                mode: cella_network::NetworkMode::Denylist,
                rules: vec![cella_network::NetworkRule {
                    domain: domain.to_string(),
                    action: cella_network::RuleAction::Block,
                    ..Default::default()
                }],
                ..Default::default()
            },
            url,
            |_| false,
        )
    }

    #[test]
    fn test_report_blocks_ipv6_literal_like_the_agent() {
        let mismatches: Vec<_> = ["::1", "[::1]"]
            .into_iter()
            .flat_map(|domain| {
                ["http://[::1]:8080/x", "http://[::1]/x", "https://[::1]/x"]
                    .into_iter()
                    .map(move |url| (domain, url, denylist_report(domain, url)))
            })
            .filter(|(_, url, lines)| lines.first() != Some(&format!("\u{2717} BLOCKED: {url}")))
            .collect();
        assert!(mismatches.is_empty(), "{mismatches:#?}");
    }

    #[test]
    fn test_report_rejects_what_the_agent_answers_400() {
        let mismatches = [
            (
                "http://example.com:65536/",
                "port is not a number from 0 to 65535",
            ),
            ("http://[::]/x", "unspecified address (0.0.0.0 or ::)"),
            (
                "http://0.0.0.0:8080/x",
                "unspecified address (0.0.0.0 or ::)",
            ),
            (
                "https://[::1%25eth0]/",
                "bracketed host is not an IPv6 address",
            ),
            ("u:p@example.com:443", "CONNECT target carries userinfo"),
            ("http:///x", "invalid format"),
        ]
        .into_iter()
        .map(|(url, reason)| {
            let expected = [
                format!("\u{2717} REJECTED: {url}"),
                format!("  the proxy answers 400 Bad Request: {reason}"),
            ];
            (denylist_report("example.com", url), expected)
        })
        .filter(|(lines, expected)| lines != expected)
        .collect::<Vec<_>>();
        assert!(mismatches.is_empty(), "{mismatches:#?}");
    }
}
