//! Glob-based rule matching engine for domain and path filtering.
//!
//! Supports domain patterns like `*.example.com` and path patterns like `/api/**`.
//! Domain matching is case-insensitive; path matching is case-sensitive.

use std::borrow::Cow;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::{NetworkConfig, NetworkMode, NetworkRule, ResponseSpec, RuleAction};

/// Evaluates network rules against request URLs.
pub struct RuleMatcher {
    mode: NetworkMode,
    rules: Vec<CompiledRule>,
}

struct CompiledRule {
    domain_parts: Vec<PatternPart>,
    path_patterns: Vec<Vec<PatternPart>>,
    /// Pre-built verdict action, so a match costs a refcount bump.
    action: VerdictAction,
    /// Whether traffic to this rule's domain must be decrypted to apply it.
    needs_mitm: bool,
    source: String,
    /// Human-readable display of the original rule (e.g., "*.example.com /admin/** (block)").
    pattern_display: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PatternPart {
    /// Match exactly this literal string.
    Literal(String),
    /// `*` — match exactly one segment.
    Star,
    /// `**` — match zero or more segments.
    DoubleStar,
}

/// Result of evaluating a URL against the rule set.
#[derive(Debug, Clone)]
pub struct RuleVerdict {
    /// What to do with the request.
    pub action: VerdictAction,
    /// Human-readable explanation (e.g., "blocked by rule: *.prod.internal").
    ///
    /// Borrowed when no rule matched — the common case for every allowed
    /// request — so the proxy's per-request path does not allocate just to
    /// carry a constant string.
    pub reason: Cow<'static, str>,
    /// The original rule pattern that matched, if any.
    pub matched_rule: Option<String>,
    /// Source of the matching rule (e.g., "cella.toml" or "devcontainer.json").
    pub source: Option<String>,
}

impl RuleVerdict {
    /// Whether the request may go upstream.
    pub const fn is_allowed(&self) -> bool {
        matches!(self.action, VerdictAction::Allow { .. })
    }
}

/// What the proxy does with a request.
#[derive(Debug, Clone)]
pub enum VerdictAction {
    /// Forward upstream, setting these headers on the response.
    Allow {
        response_headers: Option<Arc<[(String, String)]>>,
    },
    /// Refuse the request.
    Block,
    /// Answer with the rule's next configured response.
    Respond(Arc<RespondRule>),
}

/// The responses of one `respond` rule and how many have been served.
///
/// The counter is shared by every request that hits the rule, so a sequence
/// advances across connections.
#[derive(Debug)]
pub struct RespondRule {
    responses: Vec<ResponseSpec>,
    served: AtomicUsize,
}

impl RespondRule {
    /// `None` for an empty list, which has nothing to serve.
    fn new(responses: &[ResponseSpec]) -> Option<Self> {
        (!responses.is_empty()).then(|| Self {
            responses: responses.to_vec(),
            served: AtomicUsize::new(0),
        })
    }

    /// The response to serve now: responses go out in order and the last one
    /// repeats.
    pub fn next(&self) -> &ResponseSpec {
        let served = self.served.fetch_add(1, Ordering::Relaxed);
        &self.responses[served.min(self.responses.len() - 1)]
    }

    /// Every configured response, in serving order.
    pub fn responses(&self) -> &[ResponseSpec] {
        &self.responses
    }
}

impl RuleMatcher {
    /// Build a matcher from a network configuration.
    pub fn new(config: &NetworkConfig) -> Self {
        Self::with_source(config, "config")
    }

    /// Build a matcher from config with a source label for all rules.
    pub fn with_source(config: &NetworkConfig, source: &str) -> Self {
        let rules = config
            .rules
            .iter()
            .map(|r| compile_rule(r, source))
            .collect();
        Self {
            mode: config.mode,
            rules,
        }
    }

    /// Evaluate whether a request to the given domain and path is allowed.
    ///
    /// Domain matching is case-insensitive. Path matching is case-sensitive.
    pub fn evaluate(&self, domain: &str, path: &str) -> RuleVerdict {
        let domain_lower = ascii_lowercase_cow(domain);
        let path = if path.is_empty() { "/" } else { path };

        // Both splits are loop-invariant — the domain and path do not change
        // as we walk the rules — so doing them inside the loop cost one Vec
        // per rule per request. The path split stays lazy because a rule set
        // of domain-only rules never needs it.
        let labels = split_domain(&domain_lower);
        let mut segments: Option<Vec<&str>> = None;

        for rule in &self.rules {
            if !match_segments(&rule.domain_parts, &labels) {
                continue;
            }

            // Domain matched. Check paths if any are specified.
            let path_matched = if rule.path_patterns.is_empty() {
                true
            } else {
                let segments = segments.get_or_insert_with(|| split_path(path));
                rule.path_patterns
                    .iter()
                    .any(|pp| match_segments_with_doublestar(pp, segments))
            };

            if path_matched {
                return rule.verdict();
            }
        }

        self.default_verdict()
    }

    /// Evaluate only domain-level rules (rules without path patterns).
    ///
    /// Used for CONNECT requests when MITM is unavailable — path-level rules
    /// are skipped entirely rather than evaluated against a fake path. A
    /// `respond` rule cannot be applied without decrypting, so a domain match
    /// on one blocks, whatever its paths, instead of tunnelling to the real
    /// server.
    pub fn evaluate_domain_only(&self, domain: &str) -> RuleVerdict {
        let domain_lower = ascii_lowercase_cow(domain);
        let labels = split_domain(&domain_lower);

        for rule in &self.rules {
            if !match_segments(&rule.domain_parts, &labels) {
                continue;
            }
            if matches!(rule.action, VerdictAction::Respond(_)) {
                return RuleVerdict {
                    action: VerdictAction::Block,
                    reason: format!(
                        "respond rule requires TLS interception: {}",
                        rule.pattern_display
                    )
                    .into(),
                    matched_rule: Some(rule.pattern_display.clone()),
                    source: Some(rule.source.clone()),
                };
            }
            if !rule.path_patterns.is_empty() {
                continue;
            }
            return rule.verdict();
        }

        self.default_verdict()
    }

    /// Check whether any rule for the given domain needs the request decrypted:
    /// it inspects paths, answers with a configured response, or sets
    /// response headers.
    ///
    /// If `true`, MITM TLS interception is needed for this domain.
    /// If `false`, a domain-level verdict is sufficient (no MITM).
    pub fn domain_needs_mitm(&self, domain: &str) -> bool {
        let domain_lower = ascii_lowercase_cow(domain);
        let labels = split_domain(&domain_lower);
        self.rules
            .iter()
            .any(|rule| rule.needs_mitm && match_segments(&rule.domain_parts, &labels))
    }

    /// Verdict when no rule matched. Allocation-free: this is the path every
    /// unmatched request takes.
    const fn default_verdict(&self) -> RuleVerdict {
        match self.mode {
            NetworkMode::Denylist => RuleVerdict {
                action: VerdictAction::Allow {
                    response_headers: None,
                },
                reason: Cow::Borrowed("allowed (no matching deny rule)"),
                matched_rule: None,
                source: None,
            },
            NetworkMode::Allowlist => RuleVerdict {
                action: VerdictAction::Block,
                reason: Cow::Borrowed("blocked (no matching allow rule)"),
                matched_rule: None,
                source: None,
            },
        }
    }
}

impl CompiledRule {
    /// Verdict for a request this rule matched.
    fn verdict(&self) -> RuleVerdict {
        let outcome = match self.action {
            VerdictAction::Allow { .. } => "allowed",
            VerdictAction::Block => "blocked",
            VerdictAction::Respond(_) => "responded",
        };
        RuleVerdict {
            action: self.action.clone(),
            reason: format!("{outcome} by rule: {}", self.pattern_display).into(),
            matched_rule: Some(self.pattern_display.clone()),
            source: Some(self.source.clone()),
        }
    }
}
/// Lowercase `s` only if it actually contains uppercase ASCII.
///
/// Hostnames arrive lowercase in the overwhelming majority of requests, so the
/// unconditional `to_ascii_lowercase` this replaces allocated a `String` per
/// request for nothing.
fn ascii_lowercase_cow(s: &str) -> Cow<'_, str> {
    if s.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(s.to_ascii_lowercase())
    } else {
        Cow::Borrowed(s)
    }
}

/// Split a lowercase domain into its labels.
fn split_domain(domain: &str) -> Vec<&str> {
    domain.split('.').collect()
}

/// Split a path into non-empty segments.
fn split_path(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

fn compile_rule(rule: &NetworkRule, source: &str) -> CompiledRule {
    let domain_parts = parse_domain_pattern(&rule.domain);
    let path_patterns = rule.paths.iter().map(|p| parse_path_pattern(p)).collect();

    let action_str = match rule.action {
        RuleAction::Block => "block",
        RuleAction::Allow => "allow",
        RuleAction::Respond => "respond",
    };
    let pattern_display = if rule.paths.is_empty() {
        format!("{} ({})", rule.domain, action_str)
    } else {
        format!("{} {} ({})", rule.domain, rule.paths.join(", "), action_str)
    };

    CompiledRule {
        domain_parts,
        path_patterns,
        action: compile_action(rule),
        needs_mitm: rule.needs_mitm(),
        source: source.to_string(),
        pattern_display,
    }
}

/// Canonical form of an IPv6 literal written with or without brackets.
///
/// An IPv4-mapped address (`::ffff:a.b.c.d`) becomes dotted IPv4; any other
/// address, IPv4-compatible (`::a.b.c.d`) included, takes the compressed
/// lowercase IPv6 form. Returns `None` when `literal` is not an IPv6 address,
/// including one with a zone ID.
pub fn canonical_ipv6_literal(literal: &str) -> Option<String> {
    let inner = literal
        .strip_prefix('[')
        .and_then(|l| l.strip_suffix(']'))
        .unwrap_or(literal);
    let addr: Ipv6Addr = inner.parse().ok()?;
    Some(canonical_ipv6(addr).to_string())
}

/// `addr` as the address family it actually reaches.
///
/// Only an IPv4-mapped address reaches IPv4. The deprecated IPv4-compatible
/// form (RFC 4291 §2.5.5.1) is an ordinary IPv6 destination, so it stays
/// IPv6.
const fn canonical_ipv6(addr: Ipv6Addr) -> IpAddr {
    match addr.to_ipv4_mapped() {
        Some(v4) => IpAddr::V4(v4),
        None => IpAddr::V6(addr),
    }
}

/// Build the verdict action a match on `rule` returns.
///
/// A `respond` rule with no responses has nothing to serve and blocks;
/// config validation rejects it before it gets here.
fn compile_action(rule: &NetworkRule) -> VerdictAction {
    match rule.action {
        RuleAction::Block => VerdictAction::Block,
        RuleAction::Allow => VerdictAction::Allow {
            response_headers: (!rule.response_headers.is_empty()).then(|| {
                rule.response_headers
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect()
            }),
        },
        RuleAction::Respond => RespondRule::new(&rule.responses)
            .map_or(VerdictAction::Block, |r| {
                VerdictAction::Respond(Arc::new(r))
            }),
    }
}

/// Parse a domain pattern like `*.example.com` into parts.
/// Splits on `.` and maps `*` to `Star`. An IPv6 literal, with or without
/// brackets, compiles to its canonical form: one literal part for an IPv6
/// address, or the labels of the IPv4 address it maps to.
fn parse_domain_pattern(pattern: &str) -> Vec<PatternPart> {
    if let Some(canonical) = canonical_ipv6_literal(pattern) {
        if canonical.contains(':') {
            return vec![PatternPart::Literal(canonical)];
        }
        return parse_domain_pattern(&canonical);
    }
    pattern
        .to_ascii_lowercase()
        .split('.')
        .map(|segment| {
            if segment == "*" {
                PatternPart::Star
            } else {
                PatternPart::Literal(segment.to_string())
            }
        })
        .collect()
}

/// Parse a path pattern like `/v1/admin/**` into parts.
/// Splits on `/` (ignoring empty segments from leading slash) and maps
/// `*` to `Star`, `**` to `DoubleStar`.
fn parse_path_pattern(pattern: &str) -> Vec<PatternPart> {
    pattern
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|segment| match segment {
            "*" => PatternPart::Star,
            "**" => PatternPart::DoubleStar,
            other => PatternPart::Literal(other.to_string()),
        })
        .collect()
}

/// Exact segment matching (for domains): `*` matches one, no `**` support.
fn match_segments(pattern: &[PatternPart], segments: &[&str]) -> bool {
    if pattern.len() != segments.len() {
        return false;
    }
    pattern.iter().zip(segments.iter()).all(|(p, s)| match p {
        PatternPart::Literal(lit) => lit == s,
        PatternPart::Star | PatternPart::DoubleStar => true,
    })
}

/// Segment matching with `**` support (for paths).
fn match_segments_with_doublestar(pattern: &[PatternPart], segments: &[&str]) -> bool {
    // Only `**` can make a pattern match a different number of segments than
    // it has parts. Without one the match is positional, so it needs neither
    // the recursion nor the memo set that guards it against `**` blowup —
    // and the memo set is an allocation on every path check.
    if !pattern.contains(&PatternPart::DoubleStar) {
        return match_segments(pattern, segments);
    }

    let mut visited = std::collections::HashSet::new();
    match_recursive(pattern, segments, 0, 0, &mut visited)
}

fn match_recursive(
    pattern: &[PatternPart],
    segments: &[&str],
    pi: usize,
    si: usize,
    visited: &mut std::collections::HashSet<(usize, usize)>,
) -> bool {
    if !visited.insert((pi, si)) {
        return false;
    }

    // Both exhausted: match.
    if pi == pattern.len() && si == segments.len() {
        return true;
    }

    // Pattern exhausted but segments remain: no match.
    if pi == pattern.len() {
        return false;
    }

    match &pattern[pi] {
        PatternPart::DoubleStar => {
            // `**` matches zero or more segments.
            for skip in 0..=(segments.len() - si) {
                if match_recursive(pattern, segments, pi + 1, si + skip, visited) {
                    return true;
                }
            }
            false
        }
        PatternPart::Star => {
            if si < segments.len() {
                match_recursive(pattern, segments, pi + 1, si + 1, visited)
            } else {
                false
            }
        }
        PatternPart::Literal(lit) => {
            if si < segments.len() && lit == segments[si] {
                match_recursive(pattern, segments, pi + 1, si + 1, visited)
            } else {
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{NetworkConfig, NetworkMode, NetworkRule, RuleAction};

    use super::*;

    /// Match a domain (lowercase) against a compiled domain pattern.
    ///
    /// `*` matches exactly one domain label.
    /// `*.example.com` matches `foo.example.com` but NOT `foo.bar.example.com`.
    ///
    /// `evaluate` splits the domain once per request rather than once per
    /// rule, so this string-taking form only exists for the tests below.
    fn match_domain(pattern: &[PatternPart], domain: &str) -> bool {
        match_segments(pattern, &split_domain(domain))
    }

    /// Match a path against a compiled path pattern.
    ///
    /// `*` matches exactly one path segment.
    /// `**` matches zero or more path segments.
    fn match_path(pattern: &[PatternPart], path: &str) -> bool {
        match_segments_with_doublestar(pattern, &split_path(path))
    }

    // --- Domain pattern matching ---

    #[test]
    fn ipv6_literal_rule_matches_canonical_form() {
        for pattern in ["::1", "[::1]", "0:0:0:0:0:0:0:1", "[0:0::1]"] {
            let parts = parse_domain_pattern(pattern);
            assert_eq!(
                parts,
                [PatternPart::Literal("::1".to_string())],
                "{pattern}"
            );
            assert!(match_domain(&parts, "::1"), "{pattern}");
        }
        let parts = parse_domain_pattern("[2001:DB8:0::1]");
        assert!(match_domain(&parts, "2001:db8::1"));
    }

    #[test]
    fn ipv4_mapped_rule_matches_ipv4_address() {
        for pattern in ["::ffff:127.0.0.1", "[::ffff:7f00:1]"] {
            let parts = parse_domain_pattern(pattern);
            assert!(match_domain(&parts, "127.0.0.1"), "{pattern}");
        }
    }

    #[test]
    fn ipv4_compatible_rule_stays_ipv6() {
        for pattern in ["::127.0.0.1", "[::7f00:1]"] {
            let parts = parse_domain_pattern(pattern);
            assert_eq!(
                parts,
                [PatternPart::Literal("::7f00:1".to_string())],
                "{pattern}"
            );
            assert!(!match_domain(&parts, "127.0.0.1"), "{pattern}");
        }
        let parts = parse_domain_pattern("127.0.0.1");
        assert!(!match_domain(&parts, "::7f00:1"));
    }

    #[test]
    fn canonical_ipv6_literal_forms() {
        assert_eq!(canonical_ipv6_literal("[::1]").as_deref(), Some("::1"));
        assert_eq!(
            canonical_ipv6_literal("::FFFF:127.0.0.1").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(
            canonical_ipv6_literal("[::ffff:127.0.0.1]").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(
            canonical_ipv6_literal("[::127.0.0.1]").as_deref(),
            Some("::7f00:1")
        );
        assert_eq!(
            canonical_ipv6_literal("::8.8.8.8").as_deref(),
            Some("::808:808")
        );
        assert_eq!(canonical_ipv6_literal("::").as_deref(), Some("::"));
        assert_eq!(canonical_ipv6_literal("::1%eth0"), None);
        assert_eq!(canonical_ipv6_literal("[not-an-ip]"), None);
        assert_eq!(canonical_ipv6_literal("127.0.0.1"), None);
        assert_eq!(canonical_ipv6_literal("example.com"), None);
    }

    #[test]
    fn exact_domain_match() {
        let parts = parse_domain_pattern("example.com");
        assert!(match_domain(&parts, "example.com"));
        assert!(!match_domain(&parts, "foo.example.com"));
        assert!(!match_domain(&parts, "other.com"));
    }

    #[test]
    fn wildcard_subdomain() {
        let parts = parse_domain_pattern("*.example.com");
        assert!(match_domain(&parts, "foo.example.com"));
        assert!(match_domain(&parts, "bar.example.com"));
        assert!(!match_domain(&parts, "foo.bar.example.com"));
        assert!(!match_domain(&parts, "example.com"));
    }

    #[test]
    fn wildcard_middle_segment() {
        let parts = parse_domain_pattern("api.*.internal");
        assert!(match_domain(&parts, "api.foo.internal"));
        assert!(match_domain(&parts, "api.bar.internal"));
        assert!(!match_domain(&parts, "api.foo.bar.internal"));
        assert!(!match_domain(&parts, "web.foo.internal"));
    }

    #[test]
    fn domain_case_insensitive() {
        // Patterns are lowercased during parsing, domains are lowercased
        // during evaluation (in `evaluate()`). Direct `match_domain` calls
        // expect a pre-lowercased domain.
        let parts = parse_domain_pattern("*.Example.COM");
        assert!(match_domain(&parts, "foo.example.com"));

        // Full evaluation path handles case normalization:
        let config = NetworkConfig {
            mode: NetworkMode::Denylist,
            rules: vec![NetworkRule {
                domain: "*.Example.COM".to_string(),
                paths: vec![],
                action: RuleAction::Block,
                ..Default::default()
            }],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);
        let v = matcher.evaluate("FOO.EXAMPLE.COM", "/");
        assert!(!v.is_allowed());
    }

    // --- Path pattern matching ---

    #[test]
    fn exact_path_match() {
        let parts = parse_path_pattern("/api/v1");
        assert!(match_path(&parts, "/api/v1"));
        assert!(!match_path(&parts, "/api/v2"));
        assert!(!match_path(&parts, "/api/v1/extra"));
    }

    #[test]
    fn single_star_path() {
        let parts = parse_path_pattern("/api/*");
        assert!(match_path(&parts, "/api/users"));
        assert!(match_path(&parts, "/api/posts"));
        assert!(!match_path(&parts, "/api/users/123"));
        assert!(!match_path(&parts, "/api"));
    }

    #[test]
    fn double_star_path() {
        let parts = parse_path_pattern("/v1/admin/**");
        assert!(match_path(&parts, "/v1/admin"));
        assert!(match_path(&parts, "/v1/admin/users"));
        assert!(match_path(&parts, "/v1/admin/users/123/roles"));
        assert!(!match_path(&parts, "/v1/public"));
        assert!(!match_path(&parts, "/v2/admin"));
    }

    #[test]
    fn double_star_middle() {
        let parts = parse_path_pattern("/api/**/delete");
        assert!(match_path(&parts, "/api/delete"));
        assert!(match_path(&parts, "/api/users/delete"));
        assert!(match_path(&parts, "/api/users/123/delete"));
        assert!(!match_path(&parts, "/api/users/update"));
    }

    /// `match_segments_with_doublestar` short-circuits to `match_segments`
    /// when a pattern has no `**`. That is only sound if the two agree on
    /// every such pattern, so check them against each other directly rather
    /// than trusting a handful of spot cases.
    #[test]
    fn doublestar_free_patterns_agree_with_positional_matching() {
        let patterns = ["/api", "/api/*", "/*", "/*/*", "/a/*/c", ""];
        let paths = [
            "",
            "/",
            "/api",
            "/api/",
            "/api/users",
            "/api/users/123",
            "/a/b/c",
            "/a/b",
            "/x",
        ];

        for pattern in patterns {
            let parts = parse_path_pattern(pattern);
            assert!(
                !parts.contains(&PatternPart::DoubleStar),
                "fixture must stay `**`-free: {pattern}"
            );
            for path in paths {
                let segments = split_path(path);
                let mut visited = std::collections::HashSet::new();
                let recursive = match_recursive(&parts, &segments, 0, 0, &mut visited);
                assert_eq!(
                    match_segments(&parts, &segments),
                    recursive,
                    "pattern {pattern:?} vs path {path:?}"
                );
            }
        }
    }

    // --- Rule evaluation ---

    #[test]
    fn denylist_blocks_matching() {
        let config = NetworkConfig {
            mode: NetworkMode::Denylist,
            rules: vec![NetworkRule {
                domain: "*.prod.internal".to_string(),
                paths: vec![],
                action: RuleAction::Block,
                ..Default::default()
            }],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);

        let v = matcher.evaluate("api.prod.internal", "/");
        assert!(!v.is_allowed());

        let v = matcher.evaluate("api.staging.internal", "/");
        assert!(v.is_allowed());
    }

    #[test]
    fn allowlist_blocks_unmatched() {
        let config = NetworkConfig {
            mode: NetworkMode::Allowlist,
            rules: vec![NetworkRule {
                domain: "registry.npmjs.org".to_string(),
                paths: vec![],
                action: RuleAction::Allow,
                ..Default::default()
            }],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);

        let v = matcher.evaluate("registry.npmjs.org", "/");
        assert!(v.is_allowed());

        let v = matcher.evaluate("evil.example.com", "/");
        assert!(!v.is_allowed());
    }

    #[test]
    fn path_level_blocking() {
        let config = NetworkConfig {
            mode: NetworkMode::Denylist,
            rules: vec![NetworkRule {
                domain: "api.example.com".to_string(),
                paths: vec!["/v1/admin/*".to_string(), "/internal/**".to_string()],
                action: RuleAction::Block,
                ..Default::default()
            }],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);

        let v = matcher.evaluate("api.example.com", "/v1/admin/delete");
        assert!(!v.is_allowed());

        let v = matcher.evaluate("api.example.com", "/internal/secrets/key");
        assert!(!v.is_allowed());

        let v = matcher.evaluate("api.example.com", "/v1/public/data");
        assert!(v.is_allowed());
    }

    #[test]
    fn domain_needs_mitm_for_path_rules() {
        let config = NetworkConfig {
            mode: NetworkMode::Denylist,
            rules: vec![
                NetworkRule {
                    domain: "*.prod.internal".to_string(),
                    paths: vec![],
                    action: RuleAction::Block,
                    ..Default::default()
                },
                NetworkRule {
                    domain: "api.example.com".to_string(),
                    paths: vec!["/admin/**".to_string()],
                    action: RuleAction::Block,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);

        assert!(!matcher.domain_needs_mitm("foo.prod.internal"));
        assert!(matcher.domain_needs_mitm("api.example.com"));
        assert!(!matcher.domain_needs_mitm("other.example.com"));
    }

    #[test]
    fn first_matching_rule_wins() {
        let config = NetworkConfig {
            mode: NetworkMode::Denylist,
            rules: vec![
                NetworkRule {
                    domain: "api.example.com".to_string(),
                    paths: vec!["/public/**".to_string()],
                    action: RuleAction::Allow,
                    ..Default::default()
                },
                NetworkRule {
                    domain: "api.example.com".to_string(),
                    paths: vec![],
                    action: RuleAction::Block,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);

        // /public/data matches first rule (allow)
        let v = matcher.evaluate("api.example.com", "/public/data");
        assert!(v.is_allowed());

        // /secret matches second rule (block)
        let v = matcher.evaluate("api.example.com", "/secret");
        assert!(!v.is_allowed());
    }

    #[test]
    fn rule_verdict_shows_pattern_not_source() {
        let config = NetworkConfig {
            mode: NetworkMode::Denylist,
            rules: vec![NetworkRule {
                domain: "*.prod.internal".to_string(),
                paths: vec!["/admin/**".to_string()],
                action: RuleAction::Block,
                ..Default::default()
            }],
            ..Default::default()
        };
        let matcher = RuleMatcher::with_source(&config, "cella.toml");

        let v = matcher.evaluate("api.prod.internal", "/admin/users");
        assert!(!v.is_allowed());
        assert_eq!(
            v.matched_rule.as_deref(),
            Some("*.prod.internal /admin/** (block)")
        );
        assert_eq!(v.source.as_deref(), Some("cella.toml"));
        assert!(v.reason.contains("*.prod.internal"));
    }

    #[test]
    fn memoization_handles_pathological_pattern() {
        // Multiple ** patterns that could cause exponential blowup without memoization.
        let parts = parse_path_pattern("/**/a/**/b/**/c");
        // Should complete quickly (not hang) regardless of match result.
        assert!(match_path(&parts, "/x/y/a/z/b/w/c"));
        assert!(!match_path(&parts, "/x/y/a/z/b/w/d"));

        // Long path with multiple ** — tests memoization prevents exponential time.
        let parts = parse_path_pattern("/**/x/**/y/**/z");
        assert!(match_path(&parts, "/a/b/c/x/d/e/f/y/g/h/i/j/k/l/m/z"));
    }

    #[test]
    fn empty_path_defaults_to_root() {
        let config = NetworkConfig {
            mode: NetworkMode::Denylist,
            rules: vec![NetworkRule {
                domain: "example.com".to_string(),
                paths: vec![],
                action: RuleAction::Block,
                ..Default::default()
            }],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);

        let v = matcher.evaluate("example.com", "");
        assert!(!v.is_allowed());
    }

    #[test]
    fn evaluate_domain_only_skips_path_rules() {
        let config = NetworkConfig {
            mode: NetworkMode::Denylist,
            rules: vec![NetworkRule {
                domain: "example.com".to_string(),
                paths: vec!["/**".to_string()],
                action: RuleAction::Block,
                ..Default::default()
            }],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);

        // Full evaluate with "/" matches the "/**" path rule → blocked.
        assert!(!matcher.evaluate("example.com", "/").is_allowed());
        // Domain-only skips the path rule → allowed (denylist default).
        assert!(matcher.evaluate_domain_only("example.com").is_allowed());
    }

    #[test]
    fn evaluate_domain_only_still_blocks_domain_rules() {
        let config = NetworkConfig {
            mode: NetworkMode::Denylist,
            rules: vec![
                NetworkRule {
                    domain: "evil.com".to_string(),
                    paths: vec![],
                    action: RuleAction::Block,
                    ..Default::default()
                },
                NetworkRule {
                    domain: "mixed.com".to_string(),
                    paths: vec!["/secret/**".to_string()],
                    action: RuleAction::Block,
                    ..Default::default()
                },
                NetworkRule {
                    domain: "mixed.com".to_string(),
                    paths: vec![],
                    action: RuleAction::Block,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);

        assert!(!matcher.evaluate_domain_only("evil.com").is_allowed());
        assert!(!matcher.evaluate_domain_only("mixed.com").is_allowed());
        assert!(matcher.evaluate_domain_only("good.com").is_allowed());
    }

    // --- Respond rules and response headers ---

    fn response(status: u16) -> ResponseSpec {
        ResponseSpec {
            status,
            headers: std::collections::BTreeMap::new(),
            body: String::new(),
        }
    }

    fn respond_rule(domain: &str, paths: &[&str], statuses: &[u16]) -> NetworkRule {
        NetworkRule {
            domain: domain.to_string(),
            paths: paths.iter().map(ToString::to_string).collect(),
            action: RuleAction::Respond,
            responses: statuses.iter().map(|&s| response(s)).collect(),
            ..Default::default()
        }
    }

    fn matcher_for(rules: Vec<NetworkRule>) -> RuleMatcher {
        RuleMatcher::new(&NetworkConfig {
            mode: NetworkMode::Denylist,
            rules,
            ..Default::default()
        })
    }

    #[test]
    fn respond_rule_match_returns_respond_verdict() {
        let matcher = matcher_for(vec![respond_rule(
            "api.anthropic.com",
            &["/v1/messages"],
            &[529, 200],
        )]);

        let v = matcher.evaluate("api.anthropic.com", "/v1/messages");
        let VerdictAction::Respond(rule) = &v.action else {
            panic!("expected a respond verdict, got {:?}", v.action);
        };
        assert!(!v.is_allowed());
        assert_eq!(
            rule.responses()
                .iter()
                .map(|r| r.status)
                .collect::<Vec<_>>(),
            [529, 200]
        );
        assert_eq!(
            v.reason,
            "responded by rule: api.anthropic.com /v1/messages (respond)"
        );

        // A path the rule does not cover falls through to the default.
        assert!(
            matcher
                .evaluate("api.anthropic.com", "/v1/models")
                .is_allowed()
        );
    }

    #[test]
    fn respond_rule_serves_in_order_then_repeats_last() {
        let matcher = matcher_for(vec![respond_rule("api.example.com", &[], &[529, 529, 200])]);

        let served: Vec<u16> = (0..4)
            .map(|_| match matcher.evaluate("api.example.com", "/").action {
                VerdictAction::Respond(rule) => rule.next().status,
                other => panic!("expected a respond verdict, got {other:?}"),
            })
            .collect();
        assert_eq!(served, [529, 529, 200, 200]);
    }

    #[test]
    fn allow_verdict_carries_response_headers() {
        let matcher = matcher_for(vec![NetworkRule {
            domain: "api.example.com".to_string(),
            action: RuleAction::Allow,
            response_headers: [("x-test-run".to_string(), "1".to_string())].into(),
            ..Default::default()
        }]);

        let v = matcher.evaluate("api.example.com", "/");
        let VerdictAction::Allow {
            response_headers: Some(headers),
        } = &v.action
        else {
            panic!("expected allow with headers, got {:?}", v.action);
        };
        assert_eq!(
            headers.as_ref(),
            [("x-test-run".to_string(), "1".to_string())]
        );
    }

    #[test]
    fn domain_needs_mitm_for_respond_and_response_headers() {
        let matcher = matcher_for(vec![
            respond_rule("mock.example.com", &[], &[200]),
            NetworkRule {
                domain: "headers.example.com".to_string(),
                action: RuleAction::Allow,
                response_headers: [("x-test-run".to_string(), "1".to_string())].into(),
                ..Default::default()
            },
            NetworkRule {
                domain: "blocked.example.com".to_string(),
                action: RuleAction::Block,
                ..Default::default()
            },
        ]);

        assert!(matcher.domain_needs_mitm("mock.example.com"));
        assert!(matcher.domain_needs_mitm("headers.example.com"));
        assert!(!matcher.domain_needs_mitm("blocked.example.com"));
        assert!(!matcher.domain_needs_mitm("other.example.com"));
    }

    #[test]
    fn evaluate_domain_only_blocks_respond_rule_with_paths() {
        let matcher = matcher_for(vec![respond_rule(
            "api.anthropic.com",
            &["/v1/messages"],
            &[529],
        )]);

        let v = matcher.evaluate_domain_only("api.anthropic.com");
        assert!(matches!(v.action, VerdictAction::Block));
        assert_eq!(
            v.reason,
            "respond rule requires TLS interception: api.anthropic.com /v1/messages (respond)"
        );
        assert_eq!(
            v.matched_rule.as_deref(),
            Some("api.anthropic.com /v1/messages (respond)")
        );
        assert!(matcher.evaluate_domain_only("other.com").is_allowed());
    }
}
