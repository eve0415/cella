//! Mount-list utilities for the compose `up` path.
//!
//! Provides target-path dedup against resolved compose config and adapters
//! for converting `EnvForwarding` and `MountConfig` inputs into `MountSpec`.
//! Mount assembly (combining all sources) lives in `compose_up.rs`.

use cella_backend::{MountConfig, MountSpec};
use cella_compose::config::ResolvedComposeConfig;
use cella_env::EnvForwarding;

/// Return `true` if `candidate` is equal to `base` or is a descendant path of it.
///
/// Trailing slashes on `base` are normalised before comparison so that
/// `/root/.claude/` and `/root/.claude` are treated identically.
fn is_descendant_or_equal(candidate: &str, base: &str) -> bool {
    let base = base.trim_end_matches('/');
    candidate == base || candidate.starts_with(&format!("{base}/"))
}

/// Remove candidate mounts whose target path is equal to, or a descendant of,
/// any target already declared in the primary service's resolved volumes.
///
/// A second pass then removes cella-side candidates that conflict with an
/// *earlier* cella candidate (first-wins), using the same ancestor-or-equal
/// rule so that e.g. a bind at `/root/.foo` blocks a tmpfs at `/root/.foo/sub`.
pub fn dedup_against_base(
    resolved: &ResolvedComposeConfig,
    primary_service: &str,
    candidates: Vec<MountSpec>,
) -> Vec<MountSpec> {
    let base_targets = extract_service_targets(resolved, primary_service);

    // Pass 1: drop candidates covered by any base target.
    let after_base: Vec<MountSpec> = candidates
        .into_iter()
        .filter(|spec| {
            !base_targets
                .iter()
                .any(|base| is_descendant_or_equal(&spec.target, base))
        })
        .collect();

    // Pass 2: dedup cella candidates against each other — first declaration wins.
    let mut accepted: Vec<MountSpec> = Vec::with_capacity(after_base.len());
    for candidate in after_base {
        let blocked = accepted
            .iter()
            .any(|a| is_descendant_or_equal(&candidate.target, &a.target));
        if !blocked {
            accepted.push(candidate);
        }
    }
    accepted
}

fn extract_service_targets(resolved: &ResolvedComposeConfig, service: &str) -> Vec<String> {
    let Some(svc) = resolved.services.get(service) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for v in &svc.volumes {
        match v {
            serde_json::Value::String(s) => {
                // Short form "host:target[:opts]" — target is the second field.
                // (Defensive: docker compose config --format json normalizes to objects;
                // this branch handles caller-supplied or hand-written test fixtures.)
                let parts: Vec<&str> = s.splitn(3, ':').collect();
                if parts.len() >= 2 {
                    out.push(parts[1].to_string());
                }
            }
            serde_json::Value::Object(obj) => {
                if let Some(t) = obj.get("target").and_then(serde_json::Value::as_str) {
                    out.push(t.to_string());
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
            read_only: false,
        }];
        let specs = mount_configs_to_specs(&configs);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].source, "/h");
        assert_eq!(specs[0].target, "/c");
    }

    #[test]
    fn to_compose_yaml_bind_readonly_via_from_mount_config() {
        // Round-trip: MountConfig with read_only:true → MountSpec → YAML emits read_only: true.
        let config = MountConfig {
            mount_type: "bind".to_string(),
            source: "/host/data".to_string(),
            target: "/container/data".to_string(),
            consistency: None,
            read_only: true,
        };
        let spec = MountSpec::from_mount_config(&config);
        assert!(
            spec.read_only,
            "read_only must survive the MountConfig→MountSpec conversion"
        );
        let yaml = spec.to_compose_yaml_entry("    ");
        assert!(
            yaml.contains("read_only: true"),
            "emitted YAML must include read_only: true, got:\n{yaml}"
        );
    }

    // -----------------------------------------------------------------------
    // Finding 3: ancestor-path dedup
    // -----------------------------------------------------------------------

    #[test]
    fn dedup_drops_descendant_of_base_mount() {
        // Base has `/root/.claude`; candidate targets `/root/.claude/plugins` — must be dropped.
        let resolved = make_resolved(
            "app",
            vec![json!({"type": "bind", "source": "/host/claude", "target": "/root/.claude"})],
        );
        let candidates = vec![MountSpec::tmpfs("/root/.claude/plugins")];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert!(
            result.is_empty(),
            "descendant of base target must be dropped, got: {result:?}"
        );
    }

    #[test]
    fn dedup_does_not_drop_sibling_of_base_mount() {
        // `/root/.claude-plus` is NOT a descendant of `/root/.claude`.
        let resolved = make_resolved(
            "app",
            vec![json!({"type": "bind", "source": "/host/claude", "target": "/root/.claude"})],
        );
        let candidates = vec![MountSpec::bind("/host/claude-plus", "/root/.claude-plus")];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert_eq!(
            result.len(),
            1,
            "sibling path must not be dropped, got: {result:?}"
        );
    }

    #[test]
    fn dedup_handles_trailing_slash_in_base() {
        // Base target has a trailing slash; descendant must still be dropped.
        let resolved = make_resolved(
            "app",
            vec![json!({"type": "bind", "source": "/h", "target": "/root/.claude/"})],
        );
        let candidates = vec![MountSpec::tmpfs("/root/.claude/plugins")];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert!(
            result.is_empty(),
            "trailing slash on base must not defeat ancestor matching, got: {result:?}"
        );
    }

    #[test]
    fn dedup_first_candidate_wins_on_internal_collision() {
        // Two cella candidates with the same target: only the first survives.
        let resolved = make_resolved("app", vec![]);
        let candidates = vec![
            MountSpec::bind("/a", "/root/.foo"),
            MountSpec::tmpfs("/root/.foo"),
        ];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].source, "/a", "first candidate must win");
    }

    #[test]
    fn dedup_parent_candidate_drops_child_candidate() {
        // Parent bind at `/root/.foo` must block a child tmpfs at `/root/.foo/sub`.
        let resolved = make_resolved("app", vec![]);
        let candidates = vec![
            MountSpec::bind("/a", "/root/.foo"),
            MountSpec::tmpfs("/root/.foo/sub"),
        ];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].target, "/root/.foo",
            "parent must survive; child must be dropped"
        );
    }

    // -----------------------------------------------------------------------
    // is_descendant_or_equal unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn is_descendant_equal_exact_match() {
        assert!(is_descendant_or_equal("/root/.claude", "/root/.claude"));
    }

    #[test]
    fn is_descendant_or_equal_child() {
        assert!(is_descendant_or_equal(
            "/root/.claude/plugins",
            "/root/.claude"
        ));
    }

    #[test]
    fn is_descendant_or_equal_sibling_is_false() {
        assert!(!is_descendant_or_equal(
            "/root/.claude-plus",
            "/root/.claude"
        ));
    }

    #[test]
    fn is_descendant_or_equal_trailing_slash_on_base() {
        assert!(is_descendant_or_equal(
            "/root/.claude/plugins",
            "/root/.claude/"
        ));
    }
}
