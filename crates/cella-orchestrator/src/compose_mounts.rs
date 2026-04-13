//! Mount-list utilities for the compose `up` path.
//!
//! Provides target-path dedup against resolved compose config and adapters
//! for converting `EnvForwarding` and `MountConfig` inputs into `MountSpec`.
//! Mount assembly (combining all sources) lives in `compose_up.rs`.

use std::collections::HashSet;

use cella_backend::{MountConfig, MountKind, MountSpec};
use cella_compose::config::ResolvedComposeConfig;
use cella_env::EnvForwarding;

/// Return `true` if `candidate` is equal to `base` or is a descendant path of it.
///
/// Trailing slashes on `base` are normalised before comparison so that
/// `/root/.claude/` and `/root/.claude` are treated identically.
pub(crate) fn is_descendant_or_equal(candidate: &str, base: &str) -> bool {
    let base = base.trim_end_matches('/');
    candidate == base || candidate.starts_with(&format!("{base}/"))
}

/// Filter out mount specs whose target is equal to or a descendant of the
/// reserved `agent_vol_target` (e.g., `/cella`).
///
/// The agent volume is the single most critical resource cella manages. Any
/// user or feature mount that shadows it or any subdirectory (e.g. `/cella/bin`)
/// would replace the agent binary path and break managed-agent behaviour.
///
/// Returns the filtered list; each rejected mount is logged at `warn` level.
pub(crate) fn filter_reserved_agent_subtree(
    specs: Vec<MountSpec>,
    agent_vol_target: &str,
) -> Vec<MountSpec> {
    specs
        .into_iter()
        .filter(|spec| {
            if is_descendant_or_equal(&spec.target, agent_vol_target) {
                tracing::warn!(
                    target = %spec.target,
                    source = %spec.source,
                    reserved = %agent_vol_target,
                    "mount rejected: target is inside the reserved agent subtree",
                );
                false
            } else {
                true
            }
        })
        .collect()
}

/// Remove candidate mounts whose target path is equal to, or a descendant of,
/// any target already declared in the primary service's resolved volumes.
///
/// A second pass then removes cella-side candidates that have the **exact same
/// target** as an earlier cella candidate (first-wins). Only exact-target
/// matches are dropped here — intentional parent+child overlays (e.g. a bind at
/// `/root/.claude` plus a tmpfs at `/root/.claude/plugins`) are preserved.
pub fn dedup_against_base(
    resolved: &ResolvedComposeConfig,
    primary_service: &str,
    candidates: Vec<MountSpec>,
) -> Vec<MountSpec> {
    let base_targets = extract_service_targets(resolved, primary_service);

    // Pass 1: drop candidates covered by any base target (ancestor-or-equal).
    //
    // Tmpfs mounts are cella's isolation mechanism for subdirectories (e.g.
    // `.claude/plugins` under a user-owned `~/.claude` bind). Dropping them on
    // ancestor-match would silently defeat that isolation. Instead, tmpfs is
    // only dropped when the target EXACTLY matches a base target (the base
    // already owns that path entirely).
    let after_base: Vec<MountSpec> = candidates
        .into_iter()
        .filter(|spec| {
            if spec.kind == MountKind::Tmpfs {
                !base_targets.contains(&spec.target)
            } else {
                !base_targets
                    .iter()
                    .any(|base| is_descendant_or_equal(&spec.target, base))
            }
        })
        .collect();

    // Pass 2: dedup cella candidates against each other — exact target match only,
    // first declaration wins. Intentional parent+child overlays are preserved.
    let mut seen_targets: HashSet<String> = HashSet::new();
    let mut accepted: Vec<MountSpec> = Vec::with_capacity(after_base.len());
    for candidate in after_base {
        if seen_targets.insert(candidate.target.clone()) {
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
///
/// Configs with unrecognised mount types are skipped with a warning rather than
/// silently demoted to `bind`.
pub fn mount_configs_to_specs(configs: &[MountConfig]) -> Vec<MountSpec> {
    configs
        .iter()
        .filter_map(|mc| {
            let spec = MountSpec::from_mount_config(mc);
            if spec.is_none() {
                tracing::warn!(
                    mount_type = %mc.mount_type,
                    target = %mc.target,
                    "unsupported mount type — skipping",
                );
            }
            spec
        })
        .collect()
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
        let spec = MountSpec::from_mount_config(&config).unwrap();
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
    // Pass-1 tmpfs isolation (Finding 3)
    // -----------------------------------------------------------------------

    #[test]
    fn pass_one_preserves_tmpfs_under_base_bind() {
        // Base owns `/root/.claude` via bind; cella wants a tmpfs at
        // `/root/.claude/plugins` for isolation. The tmpfs must survive Pass-1
        // so it can shadow the subdirectory instead of leaking to host storage.
        let resolved = make_resolved(
            "app",
            vec![json!({"type": "bind", "source": "/host/claude", "target": "/root/.claude"})],
        );
        let candidates = vec![MountSpec::tmpfs("/root/.claude/plugins")];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert_eq!(
            result.len(),
            1,
            "tmpfs descendant of base bind must be preserved; got: {result:?}"
        );
        assert_eq!(result[0].target, "/root/.claude/plugins");
    }

    #[test]
    fn pass_one_drops_tmpfs_on_exact_target_match() {
        // Base already owns `/root/.claude/plugins` exactly — a cella tmpfs at
        // the same path would conflict, so it must be dropped.
        let resolved = make_resolved(
            "app",
            vec![json!({"type": "bind", "source": "/h", "target": "/root/.claude/plugins"})],
        );
        let candidates = vec![MountSpec::tmpfs("/root/.claude/plugins")];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert!(
            result.is_empty(),
            "tmpfs on exact base target must be dropped; got: {result:?}"
        );
    }

    #[test]
    fn pass_one_still_drops_bind_descendants() {
        // Non-tmpfs mounts that are descendants of a base target are still dropped.
        let resolved = make_resolved(
            "app",
            vec![json!({"type": "bind", "source": "/host/claude", "target": "/root/.claude"})],
        );
        let candidates = vec![MountSpec::bind("/foo", "/root/.claude/other")];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert!(
            result.is_empty(),
            "bind descendant of base must still be dropped; got: {result:?}"
        );
    }

    #[test]
    fn pass_one_still_drops_exact_bind_match() {
        // Non-tmpfs mounts that exactly match a base target are dropped.
        let resolved = make_resolved(
            "app",
            vec![json!({"type": "bind", "source": "/host/claude", "target": "/root/.claude"})],
        );
        let candidates = vec![MountSpec::bind("/foo", "/root/.claude")];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert!(
            result.is_empty(),
            "bind on exact base target must be dropped; got: {result:?}"
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
        // Base target has a trailing slash; a non-tmpfs descendant must still be dropped.
        let resolved = make_resolved(
            "app",
            vec![json!({"type": "bind", "source": "/h", "target": "/root/.claude/"})],
        );
        let candidates = vec![MountSpec::bind("/foo", "/root/.claude/plugins")];
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
    fn dedup_preserves_intentional_tmpfs_over_bind() {
        // Claude plugin isolation: bind at `/root/.claude` plus tmpfs at
        // `/root/.claude/plugins` are an *intentional* parent+child overlay.
        // Both must survive Pass-2 dedup (exact-match only).
        let resolved = make_resolved("app", vec![]);
        let candidates = vec![
            MountSpec::bind("/host/.claude", "/root/.claude"),
            MountSpec::tmpfs("/root/.claude/plugins"),
        ];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert_eq!(
            result.len(),
            2,
            "bind+tmpfs overlay must both survive; got: {result:?}"
        );
        assert_eq!(result[0].target, "/root/.claude");
        assert_eq!(result[1].target, "/root/.claude/plugins");
    }

    #[test]
    fn dedup_first_candidate_wins_on_exact_collision() {
        // Two candidates with the same exact target: only the first survives.
        let resolved = make_resolved("app", vec![]);
        let candidates = vec![
            MountSpec::bind("/a", "/root/.foo"),
            MountSpec::bind("/b", "/root/.foo"),
        ];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].source, "/a", "first candidate must win");
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

    // -----------------------------------------------------------------------
    // filter_reserved_agent_subtree (Finding 1)
    // -----------------------------------------------------------------------

    #[test]
    fn reject_mount_targeting_cella_root() {
        // A user mount targeting `/cella` exactly must be filtered out.
        let specs = vec![MountSpec::bind("/host-cella", "/cella")];
        let result = filter_reserved_agent_subtree(specs, "/cella");
        assert!(
            result.is_empty(),
            "mount at /cella must be rejected; got: {result:?}"
        );
    }

    #[test]
    fn reject_mount_targeting_cella_descendant() {
        // A user mount targeting `/cella/bin` (descendant) must be filtered out.
        let specs = vec![MountSpec::bind("/host-bin", "/cella/bin")];
        let result = filter_reserved_agent_subtree(specs, "/cella");
        assert!(
            result.is_empty(),
            "mount at /cella/bin must be rejected; got: {result:?}"
        );
    }

    #[test]
    fn allow_mount_with_similar_prefix() {
        // `/cellax/bin` and `/cella-other` are NOT descendants of `/cella`.
        let specs = vec![
            MountSpec::bind("/host-x", "/cellax/bin"),
            MountSpec::bind("/host-other", "/cella-other"),
        ];
        let result = filter_reserved_agent_subtree(specs, "/cella");
        assert_eq!(
            result.len(),
            2,
            "mounts at /cellax/bin and /cella-other must NOT be rejected; got: {result:?}"
        );
    }

    #[test]
    fn reject_tool_config_mount_at_cella() {
        // Defense-in-depth: even if a tool-config mount somehow targets /cella,
        // the filter must still reject it.
        let specs = vec![
            MountSpec::bind("/legit", "/workspace"),
            MountSpec::bind("/bad", "/cella"),
        ];
        let result = filter_reserved_agent_subtree(specs, "/cella");
        assert_eq!(
            result.len(),
            1,
            "only the /workspace mount should survive; got: {result:?}"
        );
        assert_eq!(result[0].target, "/workspace");
    }
}
