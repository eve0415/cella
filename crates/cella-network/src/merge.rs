//! Merge network configurations from multiple sources.
//!
//! Rules from `cella.toml` and `devcontainer.json` `customizations.cella.network`
//! are combined (union). On conflict, `cella.toml` wins for mode and per-domain
//! rule precedence.

use crate::config::{NetworkConfig, NetworkMode, NetworkRule};

/// Source label for rules from `cella.toml`.
pub const SOURCE_CELLA_TOML: &str = "cella.toml";

/// Source label for rules from `devcontainer.json` customizations.
pub const SOURCE_DEVCONTAINER: &str = "devcontainer.json";

/// A network rule tagged with its source location.
#[derive(Debug, Clone)]
pub struct LabeledRule {
    pub rule: NetworkRule,
    pub source: String,
}

/// Merge network configs from `devcontainer.json` (base) and `cella.toml` (override).
///
/// - Mode: `cella.toml` wins if set, otherwise `devcontainer.json`
/// - Proxy: `cella.toml` wins entirely if its proxy section has any explicit values
/// - Rules: union of both sources; if the same exact domain string appears in both,
///   the `cella.toml` rule takes precedence
pub fn merge_network_configs(
    devcontainer: Option<&NetworkConfig>,
    cella_toml: Option<&NetworkConfig>,
) -> MergedNetworkConfig {
    match (devcontainer, cella_toml) {
        (None, None) => MergedNetworkConfig::default(),
        (Some(dc), None) => MergedNetworkConfig {
            mode: dc.mode,
            proxy: dc.proxy.clone(),
            rules: dc
                .rules
                .iter()
                .map(|r| LabeledRule {
                    rule: r.clone(),
                    source: SOURCE_DEVCONTAINER.to_string(),
                })
                .collect(),
        },
        (None, Some(ct)) => MergedNetworkConfig {
            mode: ct.mode,
            proxy: ct.proxy.clone(),
            rules: ct
                .rules
                .iter()
                .map(|r| LabeledRule {
                    rule: r.clone(),
                    source: SOURCE_CELLA_TOML.to_string(),
                })
                .collect(),
        },
        (Some(dc), Some(ct)) => {
            // Mode: cella.toml wins
            let mode = ct.mode;

            // Proxy: cella.toml wins if it has any explicit values
            let proxy = if ct.proxy.http.is_some()
                || ct.proxy.https.is_some()
                || ct.proxy.no_proxy.is_some()
                || ct.proxy.ca_cert.is_some()
                || !ct.proxy.enabled
            {
                ct.proxy.clone()
            } else {
                dc.proxy.clone()
            };

            // Rules: union, cella.toml wins per exact domain string
            let mut rules: Vec<LabeledRule> = Vec::new();

            // Collect cella.toml domains for dedup
            let toml_domains: std::collections::HashSet<&str> =
                ct.rules.iter().map(|r| r.domain.as_str()).collect();

            // Add devcontainer rules (skip if same domain exists in cella.toml)
            for r in &dc.rules {
                if !toml_domains.contains(r.domain.as_str()) {
                    rules.push(LabeledRule {
                        rule: r.clone(),
                        source: SOURCE_DEVCONTAINER.to_string(),
                    });
                }
            }

            // Add all cella.toml rules
            for r in &ct.rules {
                rules.push(LabeledRule {
                    rule: r.clone(),
                    source: SOURCE_CELLA_TOML.to_string(),
                });
            }

            MergedNetworkConfig { mode, proxy, rules }
        }
    }
}

/// Result of merging network configs from multiple sources.
#[derive(Debug, Clone, Default)]
pub struct MergedNetworkConfig {
    pub mode: NetworkMode,
    pub proxy: crate::config::ProxyConfig,
    pub rules: Vec<LabeledRule>,
}

impl MergedNetworkConfig {
    /// Returns `true` if any blocking rules are configured.
    pub const fn has_rules(&self) -> bool {
        !self.rules.is_empty()
    }

    /// Build a `RuleMatcher` from the merged config.
    pub fn build_matcher(&self) -> crate::rules::RuleMatcher {
        let labeled: Vec<(NetworkRule, String)> = self
            .rules
            .iter()
            .map(|lr| (lr.rule.clone(), lr.source.clone()))
            .collect();
        crate::rules::RuleMatcher::from_labeled_rules(self.mode, &labeled)
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{ProxyConfig, RuleAction};

    use super::*;

    #[test]
    fn merge_none_returns_default() {
        let merged = merge_network_configs(None, None);
        assert_eq!(merged.mode, NetworkMode::Denylist);
        assert!(!merged.has_rules());
    }

    #[test]
    fn merge_devcontainer_only() {
        let dc = NetworkConfig {
            mode: NetworkMode::Allowlist,
            rules: vec![NetworkRule {
                domain: "*.prod.internal".to_string(),
                paths: vec![],
                action: RuleAction::Block,
            }],
            ..Default::default()
        };
        let merged = merge_network_configs(Some(&dc), None);
        assert_eq!(merged.mode, NetworkMode::Allowlist);
        assert_eq!(merged.rules.len(), 1);
        assert_eq!(merged.rules[0].source, SOURCE_DEVCONTAINER);
    }

    #[test]
    fn merge_toml_wins_mode() {
        let dc = NetworkConfig {
            mode: NetworkMode::Allowlist,
            ..Default::default()
        };
        let ct = NetworkConfig {
            mode: NetworkMode::Denylist,
            ..Default::default()
        };
        let merged = merge_network_configs(Some(&dc), Some(&ct));
        assert_eq!(merged.mode, NetworkMode::Denylist);
    }

    #[test]
    fn merge_union_rules_dedup_by_domain() {
        let dc = NetworkConfig {
            rules: vec![
                NetworkRule {
                    domain: "*.prod.internal".to_string(),
                    paths: vec![],
                    action: RuleAction::Block,
                },
                NetworkRule {
                    domain: "api.example.com".to_string(),
                    paths: vec![],
                    action: RuleAction::Block,
                },
            ],
            ..Default::default()
        };
        let ct = NetworkConfig {
            rules: vec![NetworkRule {
                // Same domain as devcontainer — toml wins
                domain: "api.example.com".to_string(),
                paths: vec!["/admin/**".to_string()],
                action: RuleAction::Allow,
            }],
            ..Default::default()
        };
        let merged = merge_network_configs(Some(&dc), Some(&ct));

        // *.prod.internal from devcontainer + api.example.com from toml
        assert_eq!(merged.rules.len(), 2);

        let prod_rule = merged.rules.iter().find(|r| r.rule.domain == "*.prod.internal");
        assert!(prod_rule.is_some());
        assert_eq!(prod_rule.unwrap().source, SOURCE_DEVCONTAINER);

        let api_rule = merged.rules.iter().find(|r| r.rule.domain == "api.example.com");
        assert!(api_rule.is_some());
        assert_eq!(api_rule.unwrap().source, SOURCE_CELLA_TOML);
        assert_eq!(api_rule.unwrap().rule.action, RuleAction::Allow);
    }

    #[test]
    fn merge_proxy_toml_overrides() {
        let dc = NetworkConfig {
            proxy: ProxyConfig {
                http: Some("http://dc-proxy:3128".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let ct = NetworkConfig {
            proxy: ProxyConfig {
                http: Some("http://toml-proxy:3128".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let merged = merge_network_configs(Some(&dc), Some(&ct));
        assert_eq!(merged.proxy.http.as_deref(), Some("http://toml-proxy:3128"));
    }

    #[test]
    fn merge_proxy_devcontainer_when_toml_empty() {
        let dc = NetworkConfig {
            proxy: ProxyConfig {
                http: Some("http://dc-proxy:3128".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let ct = NetworkConfig::default();
        let merged = merge_network_configs(Some(&dc), Some(&ct));
        assert_eq!(merged.proxy.http.as_deref(), Some("http://dc-proxy:3128"));
    }
}
