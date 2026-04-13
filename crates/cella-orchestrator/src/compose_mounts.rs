//! Mount-list utilities for the compose `up` path.
//!
//! Provides target-path dedup against resolved compose config and adapters
//! for converting `EnvForwarding` and `MountConfig` inputs into `MountSpec`.
//! Mount assembly (combining all sources) lives in `compose_up.rs`.

use std::collections::HashSet;

use cella_backend::{MountConfig, MountSpec};
use cella_compose::config::ResolvedComposeConfig;
use cella_env::EnvForwarding;

/// Remove candidate mounts whose target path already appears in the primary
/// service's resolved volumes list.
pub fn dedup_against_base(
    resolved: &ResolvedComposeConfig,
    primary_service: &str,
    candidates: Vec<MountSpec>,
) -> Vec<MountSpec> {
    let base_targets = extract_service_targets(resolved, primary_service);
    candidates
        .into_iter()
        .filter(|spec| !base_targets.contains(&spec.target))
        .collect()
}

fn extract_service_targets(resolved: &ResolvedComposeConfig, service: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(svc) = resolved.services.get(service) else {
        return out;
    };
    for v in &svc.volumes {
        match v {
            serde_json::Value::String(s) => {
                // Short form "host:target[:opts]" — target is the second field.
                // (Defensive: docker compose config --format json normalizes to objects;
                // this branch handles caller-supplied or hand-written test fixtures.)
                let parts: Vec<&str> = s.splitn(3, ':').collect();
                if parts.len() >= 2 {
                    out.insert(parts[1].to_string());
                }
            }
            serde_json::Value::Object(obj) => {
                if let Some(t) = obj.get("target").and_then(|x| x.as_str()) {
                    out.insert(t.to_string());
                }
            }
            _ => {}
        }
    }
    out
}

/// Convert `env_fwd.mounts` (source/target only) to `MountSpec` list.
pub fn env_fwd_to_mount_specs(fwd: &EnvForwarding) -> Vec<MountSpec> {
    fwd.mounts
        .iter()
        .map(|m| MountSpec::bind(m.source.clone(), m.target.clone()))
        .collect()
}

/// Adapt `MountConfig` → `MountSpec` (used for user `mounts:` and feature `mounts:`
/// which already parse to `MountConfig` via shared parser).
pub fn mount_configs_to_specs(configs: &[MountConfig]) -> Vec<MountSpec> {
    configs.iter().map(MountSpec::from_mount_config).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use cella_compose::config::{ResolvedComposeConfig, ResolvedService};
    use serde_json::json;

    use super::*;

    fn make_resolved(service: &str, volumes: Vec<serde_json::Value>) -> ResolvedComposeConfig {
        let mut services = HashMap::new();
        services.insert(
            service.to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes,
            },
        );
        ResolvedComposeConfig { services }
    }

    #[test]
    fn dedup_drops_matching_target_long_form() {
        let resolved = make_resolved(
            "app",
            vec![json!({"type": "bind", "source": "/host/claude", "target": "/root/.claude"})],
        );
        let candidates = vec![
            MountSpec::bind("/cella/claude", "/root/.claude"), // should be dropped
            MountSpec::bind("/cella/codex", "/root/.codex"),   // should survive
        ];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].target, "/root/.codex");
    }

    #[test]
    fn dedup_drops_matching_target_short_form() {
        let resolved = make_resolved("app", vec![json!("/host/claude:/root/.claude:ro")]);
        let candidates = vec![MountSpec::bind("/cella/claude", "/root/.claude")];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert!(result.is_empty(), "should drop the matching target");
    }

    #[test]
    fn dedup_unknown_service_returns_all_candidates() {
        let resolved = make_resolved(
            "app",
            vec![json!({"type": "bind", "source": "/x", "target": "/root/.claude"})],
        );
        let candidates = vec![MountSpec::bind("/cella/claude", "/root/.claude")];
        // "web" is not in resolved config — all candidates must pass through
        let result = dedup_against_base(&resolved, "web", candidates);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn dedup_ignores_other_services() {
        let mut services = HashMap::new();
        services.insert(
            "db".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![json!({"type": "bind", "source": "x", "target": "/root/.claude"})],
            },
        );
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
            },
        );
        let resolved = ResolvedComposeConfig { services };
        let candidates = vec![MountSpec::bind("/x", "/root/.claude")];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert_eq!(
            result.len(),
            1,
            "/root/.claude in 'db' service must not affect 'app' service"
        );
    }

    #[test]
    fn dedup_empty_base_returns_all() {
        let resolved = make_resolved("app", vec![]);
        let candidates = vec![MountSpec::bind("/a", "/a"), MountSpec::bind("/b", "/b")];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn env_fwd_to_mount_specs_converts_each() {
        use cella_env::{EnvForwarding, ForwardMount};
        let fwd = EnvForwarding {
            mounts: vec![ForwardMount {
                source: "/ssh-sock".to_string(),
                target: "/ssh-sock".to_string(),
            }],
            ..Default::default()
        };
        let specs = env_fwd_to_mount_specs(&fwd);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].source, "/ssh-sock");
        assert_eq!(specs[0].target, "/ssh-sock");
    }

    #[test]
    fn mount_configs_to_specs_round_trip_basic() {
        let configs = vec![MountConfig {
            mount_type: "bind".to_string(),
            source: "/h".to_string(),
            target: "/c".to_string(),
            consistency: None,
        }];
        let specs = mount_configs_to_specs(&configs);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].source, "/h");
        assert_eq!(specs[0].target, "/c");
    }
}
