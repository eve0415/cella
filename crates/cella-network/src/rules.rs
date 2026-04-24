//! Glob-based rule matching engine for domain and path filtering.
//!
//! Supports domain patterns like `*.example.com` and path patterns like `/api/**`.
//! Domain matching is case-insensitive; path matching is case-sensitive.

use crate::config::{NetworkConfig, NetworkMode, NetworkRule, RuleAction};

/// Evaluates network rules against request URLs.
pub struct RuleMatcher {
    mode: NetworkMode,
    rules: Vec<CompiledRule>,
}

struct CompiledRule {
    domain_parts: Vec<PatternPart>,
    path_patterns: Vec<Vec<PatternPart>>,
    action: RuleAction,
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
    /// Whether the request is allowed.
    pub allowed: bool,
    /// Human-readable explanation (e.g., "blocked by rule: *.prod.internal").
    pub reason: String,
    /// The original rule pattern that matched, if any.
    pub matched_rule: Option<String>,
    /// Source of the matching rule (e.g., "cella.toml" or "devcontainer.json").
    pub source: Option<String>,
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

    /// Build a matcher from rules that already have source labels.
    pub fn from_labeled_rules(mode: NetworkMode, rules: &[(NetworkRule, String)]) -> Self {
        let rules = rules.iter().map(|(r, src)| compile_rule(r, src)).collect();
        Self { mode, rules }
    }

    /// Evaluate whether a request to the given domain and path is allowed.
    ///
    /// Domain matching is case-insensitive. Path matching is case-sensitive.
    pub fn evaluate(&self, domain: &str, path: &str) -> RuleVerdict {
        let domain_lower = domain.to_ascii_lowercase();
        let path = if path.is_empty() { "/" } else { path };

        for rule in &self.rules {
            if !match_domain(&rule.domain_parts, &domain_lower) {
                continue;
            }

            // Domain matched. Check paths if any are specified.
            let path_matched = if rule.path_patterns.is_empty() {
                true
            } else {
                rule.path_patterns.iter().any(|pp| match_path(pp, path))
            };

            if path_matched {
                let allowed = rule.action == RuleAction::Allow;
                return RuleVerdict {
                    allowed,
                    reason: format!(
                        "{} by rule: {}",
                        if allowed { "allowed" } else { "blocked" },
                        rule.pattern_display,
                    ),
                    matched_rule: Some(rule.pattern_display.clone()),
                    source: Some(rule.source.clone()),
                };
            }
        }

        // No rule matched — use default disposition based on mode.
        match self.mode {
            NetworkMode::Denylist => RuleVerdict {
                allowed: true,
                reason: "allowed (no matching deny rule)".to_string(),
                matched_rule: None,
                source: None,
            },
            NetworkMode::Allowlist => RuleVerdict {
                allowed: false,
                reason: "blocked (no matching allow rule)".to_string(),
                matched_rule: None,
                source: None,
            },
        }
    }

    /// Evaluate only domain-level rules (rules without path patterns).
    ///
    /// Used for CONNECT requests when MITM is unavailable — path-level rules
    /// are skipped entirely rather than evaluated against a fake path.
    pub fn evaluate_domain_only(&self, domain: &str) -> RuleVerdict {
        let domain_lower = domain.to_ascii_lowercase();

        for rule in &self.rules {
            if !rule.path_patterns.is_empty() {
                continue;
            }
            if !match_domain(&rule.domain_parts, &domain_lower) {
                continue;
            }

            let allowed = rule.action == RuleAction::Allow;
            return RuleVerdict {
                allowed,
                reason: format!(
                    "{} by rule: {}",
                    if allowed { "allowed" } else { "blocked" },
                    rule.pattern_display,
                ),
                matched_rule: Some(rule.pattern_display.clone()),
                source: Some(rule.source.clone()),
            };
        }

        match self.mode {
            NetworkMode::Denylist => RuleVerdict {
                allowed: true,
                reason: "allowed (no matching deny rule)".to_string(),
                matched_rule: None,
                source: None,
            },
            NetworkMode::Allowlist => RuleVerdict {
                allowed: false,
                reason: "blocked (no matching allow rule)".to_string(),
                matched_rule: None,
                source: None,
            },
        }
    }

    /// Check whether any rule for the given domain requires path inspection.
    ///
    /// If `true`, MITM TLS interception is needed for this domain.
    /// If `false`, domain-level blocking is sufficient (no MITM).
    pub fn domain_needs_path_inspection(&self, domain: &str) -> bool {
        let domain_lower = domain.to_ascii_lowercase();
        self.rules.iter().any(|rule| {
            !rule.path_patterns.is_empty() && match_domain(&rule.domain_parts, &domain_lower)
        })
    }
}

fn compile_rule(rule: &NetworkRule, source: &str) -> CompiledRule {
    let domain_parts = parse_domain_pattern(&rule.domain);
    let path_patterns = rule.paths.iter().map(|p| parse_path_pattern(p)).collect();

    let action_str = match rule.action {
        RuleAction::Block => "block",
        RuleAction::Allow => "allow",
    };
    let pattern_display = if rule.paths.is_empty() {
        format!("{} ({})", rule.domain, action_str)
    } else {
        format!("{} {} ({})", rule.domain, rule.paths.join(", "), action_str)
    };

    CompiledRule {
        domain_parts,
        path_patterns,
        action: rule.action,
        source: source.to_string(),
        pattern_display,
    }
}

/// Parse a domain pattern like `*.example.com` into parts.
/// Splits on `.` and maps `*` to `Star`.
fn parse_domain_pattern(pattern: &str) -> Vec<PatternPart> {
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

/// Match a domain (lowercase) against a compiled domain pattern.
///
/// `*` matches exactly one domain label.
/// `*.example.com` matches `foo.example.com` but NOT `foo.bar.example.com`.
fn match_domain(pattern: &[PatternPart], domain: &str) -> bool {
    let labels: Vec<&str> = domain.split('.').collect();
    match_segments(pattern, &labels)
}

/// Match a path against a compiled path pattern.
///
/// `*` matches exactly one path segment.
/// `**` matches zero or more path segments.
fn match_path(pattern: &[PatternPart], path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match_segments_with_doublestar(pattern, &segments)
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

    // --- Domain pattern matching ---

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
            }],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);
        let v = matcher.evaluate("FOO.EXAMPLE.COM", "/");
        assert!(!v.allowed);
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

    // --- Rule evaluation ---

    #[test]
    fn denylist_blocks_matching() {
        let config = NetworkConfig {
            mode: NetworkMode::Denylist,
            rules: vec![NetworkRule {
                domain: "*.prod.internal".to_string(),
                paths: vec![],
                action: RuleAction::Block,
            }],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);

        let v = matcher.evaluate("api.prod.internal", "/");
        assert!(!v.allowed);

        let v = matcher.evaluate("api.staging.internal", "/");
        assert!(v.allowed);
    }

    #[test]
    fn allowlist_blocks_unmatched() {
        let config = NetworkConfig {
            mode: NetworkMode::Allowlist,
            rules: vec![NetworkRule {
                domain: "registry.npmjs.org".to_string(),
                paths: vec![],
                action: RuleAction::Allow,
            }],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);

        let v = matcher.evaluate("registry.npmjs.org", "/");
        assert!(v.allowed);

        let v = matcher.evaluate("evil.example.com", "/");
        assert!(!v.allowed);
    }

    #[test]
    fn path_level_blocking() {
        let config = NetworkConfig {
            mode: NetworkMode::Denylist,
            rules: vec![NetworkRule {
                domain: "api.example.com".to_string(),
                paths: vec!["/v1/admin/*".to_string(), "/internal/**".to_string()],
                action: RuleAction::Block,
            }],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);

        let v = matcher.evaluate("api.example.com", "/v1/admin/delete");
        assert!(!v.allowed);

        let v = matcher.evaluate("api.example.com", "/internal/secrets/key");
        assert!(!v.allowed);

        let v = matcher.evaluate("api.example.com", "/v1/public/data");
        assert!(v.allowed);
    }

    #[test]
    fn domain_needs_path_inspection_check() {
        let config = NetworkConfig {
            mode: NetworkMode::Denylist,
            rules: vec![
                NetworkRule {
                    domain: "*.prod.internal".to_string(),
                    paths: vec![],
                    action: RuleAction::Block,
                },
                NetworkRule {
                    domain: "api.example.com".to_string(),
                    paths: vec!["/admin/**".to_string()],
                    action: RuleAction::Block,
                },
            ],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);

        assert!(!matcher.domain_needs_path_inspection("foo.prod.internal"));
        assert!(matcher.domain_needs_path_inspection("api.example.com"));
        assert!(!matcher.domain_needs_path_inspection("other.example.com"));
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
                },
                NetworkRule {
                    domain: "api.example.com".to_string(),
                    paths: vec![],
                    action: RuleAction::Block,
                },
            ],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);

        // /public/data matches first rule (allow)
        let v = matcher.evaluate("api.example.com", "/public/data");
        assert!(v.allowed);

        // /secret matches second rule (block)
        let v = matcher.evaluate("api.example.com", "/secret");
        assert!(!v.allowed);
    }

    #[test]
    fn rule_verdict_shows_pattern_not_source() {
        let config = NetworkConfig {
            mode: NetworkMode::Denylist,
            rules: vec![NetworkRule {
                domain: "*.prod.internal".to_string(),
                paths: vec!["/admin/**".to_string()],
                action: RuleAction::Block,
            }],
            ..Default::default()
        };
        let matcher = RuleMatcher::with_source(&config, "cella.toml");

        let v = matcher.evaluate("api.prod.internal", "/admin/users");
        assert!(!v.allowed);
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
            }],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);

        let v = matcher.evaluate("example.com", "");
        assert!(!v.allowed);
    }

    #[test]
    fn evaluate_domain_only_skips_path_rules() {
        let config = NetworkConfig {
            mode: NetworkMode::Denylist,
            rules: vec![NetworkRule {
                domain: "example.com".to_string(),
                paths: vec!["/**".to_string()],
                action: RuleAction::Block,
            }],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);

        // Full evaluate with "/" matches the "/**" path rule → blocked.
        assert!(!matcher.evaluate("example.com", "/").allowed);
        // Domain-only skips the path rule → allowed (denylist default).
        assert!(matcher.evaluate_domain_only("example.com").allowed);
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
                },
                NetworkRule {
                    domain: "mixed.com".to_string(),
                    paths: vec!["/secret/**".to_string()],
                    action: RuleAction::Block,
                },
                NetworkRule {
                    domain: "mixed.com".to_string(),
                    paths: vec![],
                    action: RuleAction::Block,
                },
            ],
            ..Default::default()
        };
        let matcher = RuleMatcher::new(&config);

        assert!(!matcher.evaluate_domain_only("evil.com").allowed);
        assert!(!matcher.evaluate_domain_only("mixed.com").allowed);
        assert!(matcher.evaluate_domain_only("good.com").allowed);
    }
}
