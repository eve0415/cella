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

/// Reject user/feature mounts that would shadow or alias the reserved agent volume.
///
/// Two independent checks are applied:
///
/// 1. **Target subtree** — any mount whose target is equal to or a descendant
///    of `agent_vol_target` (e.g., `/cella` or `/cella/bin`) is rejected.
///    Such a mount would silently replace the agent binary path.
///
/// 2. **Source name alias** — any `MountKind::Volume` mount whose source name
///    equals `agent_vol_name` is rejected, regardless of target.
///    Docker would mount the *same* underlying volume at two paths; a
///    user-writable alias at (e.g.) `/tmp/agent-rw` breaks the integrity
///    boundary even though the target is outside `/cella`.
///
///    Bind and tmpfs mounts with a source string matching the agent volume name
///    are **not** rejected — they have different semantics and do not share
///    volume identity.
///
/// Returns the filtered list; each rejected mount is logged at `warn` level.
pub(crate) fn filter_reserved_agent(
    specs: Vec<MountSpec>,
    agent_vol_target: &str,
    agent_vol_name: &str,
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
                return false;
            }
            if spec.kind == MountKind::Volume && spec.source == agent_vol_name {
                tracing::warn!(
                    vol_name = %agent_vol_name,
                    target = %spec.target,
                    "mount rejected: source aliases the reserved agent volume",
                );
                return false;
            }
            true
        })
        .collect()
}

/// Normalise a mount target path by stripping trailing slashes.
///
/// The only special case is the filesystem root `"/"`: stripping its slash
/// would produce an empty string, so we return `"/"` unchanged.
///
/// This helper is applied to **both** base targets (when building the dedup
/// set) and candidate targets (when querying the set), ensuring that
/// `/root/.claude` and `/root/.claude/` are treated as the same logical path.
fn normalize_target(s: &str) -> &str {
    let trimmed = s.trim_end_matches('/');
    if trimmed.is_empty() { "/" } else { trimmed }
}

/// Remove candidate mounts whose target path exactly matches a target already
/// declared in the primary service's resolved volumes.
///
/// Pass 1 uses exact-target matching only. Descendant paths (e.g. an SSH socket
/// bind at `/tmp/cella-ssh-agent.sock` when the user base has `/tmp:/tmp`, or a
/// feature subdir bind under a user-owned parent) are intentional overlay patterns
/// and must survive. Dropping them would sever env vars that point at sockets or
/// paths inside those directories.
///
/// A second pass then removes cella-side candidates that have the **exact same
/// target** as another cella candidate (last-wins). Only exact-target matches
/// are dropped here — intentional parent+child overlays (e.g. a bind at
/// `/root/.claude` plus a tmpfs at `/root/.claude/plugins`) are preserved.
///
/// Trailing slashes are normalised on both sides in both passes via
/// [`normalize_target`], so `/root/.foo` and `/root/.foo/` resolve to the
/// same logical path and are deduplicated correctly.
///
/// Last-wins matches the merge order established by `merge_with_devcontainer`:
/// feature mounts are prepended and user mounts are appended, so a user mount
/// that collides with a feature mount on the same target correctly overrides it.
pub fn dedup_against_base(
    resolved: &ResolvedComposeConfig,
    primary_service: &str,
    candidates: Vec<MountSpec>,
) -> Vec<MountSpec> {
    let base_targets = extract_service_targets(resolved, primary_service);

    // Pass 1: drop candidates whose target EXACTLY matches a base-compose target.
    //
    // Ancestor/descendant relationships are NOT considered here. Patterns like a
    // tmpfs isolation at `/root/.claude/plugins` under a user-owned `~/.claude`
    // bind, an SSH socket bind at `/tmp/cella-ssh-agent.sock` when the user
    // declares `/tmp:/tmp`, or a feature-specific subdir overlay are all
    // legitimate and must survive to the compose file.
    //
    // Pass 2 (last-wins dedup) follows immediately: we collect into a Vec so
    // that `.rev()` is available for the reverse+filter+reverse idiom.
    // Rationale for last-wins: `merge_with_devcontainer` prepends feature mounts
    // and appends user mounts, so user mounts appear last. Last-wins ensures a
    // user's explicit devcontainer.json mount overrides an earlier feature-declared
    // mount at the same target, matching single-container behaviour.
    //
    // Candidate targets are normalised via `normalize_target` before the
    // HashSet lookup so that trailing-slash variants match their bare counterparts.
    let mut after_base: Vec<MountSpec> = candidates
        .into_iter()
        .filter(|spec| !base_targets.contains(normalize_target(&spec.target)))
        .collect();

    // Reverse in place so that iterating forward visits last candidates first,
    // then retain only the first occurrence of each normalised target (which was
    // last in the original order). Reverse again to restore original relative order.
    after_base.reverse();
    let mut seen: HashSet<String> = HashSet::new();
    after_base.retain(|spec| seen.insert(normalize_target(&spec.target).to_string()));
    after_base.reverse();
    after_base
}

/// Extract the set of normalised mount target paths declared for `service` in
/// the resolved compose config.
///
/// Only long-form object volume entries (with a `"target"` key) are recognised.
/// `docker compose config --format json` always normalises volume entries to
/// this form; short-form strings (e.g. `"host:target:opts"`) are therefore not
/// expected in production input and are silently ignored to avoid misinterpreting
/// Windows drive-letter bind paths or anonymous volumes.
fn extract_service_targets(resolved: &ResolvedComposeConfig, service: &str) -> HashSet<String> {
    let Some(svc) = resolved.services.get(service) else {
        return HashSet::new();
    };
    let mut out = HashSet::new();
    for v in &svc.volumes {
        if let serde_json::Value::Object(obj) = v
            && let Some(t) = obj.get("target").and_then(serde_json::Value::as_str)
        {
            // Normalise so that "/root/.claude/" and "/root/.claude" compare
            // equal against candidate targets, and "/" is never collapsed to "".
            out.insert(normalize_target(t).to_string());
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

    use cella_backend::MountKind;
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
        ResolvedComposeConfig {
            services,
            ..Default::default()
        }
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
    fn extract_service_targets_ignores_short_form_strings() {
        // Short-form strings are not parsed (docker compose config normalizes
        // to long-form objects). This keeps parsing simple and avoids
        // misinterpreting Windows drive-letter binds or anonymous volumes.
        let resolved = make_resolved("app", vec![json!("C:\\Users\\me\\.claude:/root/.claude")]);
        let candidates = vec![MountSpec::bind("/cella", "/root/.claude")];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert_eq!(
            result.len(),
            1,
            "short-form base entries are not parsed; candidate survives"
        );
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
        let resolved = ResolvedComposeConfig {
            services,
            ..Default::default()
        };
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
    fn dedup_last_candidate_wins_on_internal_collision() {
        // Two cella candidates with the same target: only the last survives.
        let resolved = make_resolved("app", vec![]);
        let candidates = vec![
            MountSpec::bind("/a", "/root/.foo"),
            MountSpec::tmpfs("/root/.foo"),
        ];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, MountKind::Tmpfs, "last candidate must win");
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
    fn dedup_last_candidate_wins_on_exact_collision() {
        // Two candidates with the same exact target: only the last survives.
        // This matches merge_with_devcontainer order: features prepended, users
        // appended — so the user mount (last) overrides the feature mount (first).
        let resolved = make_resolved("app", vec![]);
        let first = MountSpec::bind("/first/source", "/root/.foo");
        let last = MountSpec::bind("/last/source", "/root/.foo");
        let result = dedup_against_base(&resolved, "app", vec![first, last]);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].source, "/last/source",
            "last candidate wins on exact target"
        );
    }

    #[test]
    fn dedup_preserves_relative_order_of_non_colliding_entries() {
        // Non-colliding entries must retain their original relative order after
        // the reverse+filter+reverse pass-2 dedup.
        let resolved = make_resolved("app", vec![]);
        let a = MountSpec::bind("/a", "/target/a");
        let b = MountSpec::bind("/b", "/target/b");
        let c = MountSpec::bind("/c", "/target/c");
        let result = dedup_against_base(&resolved, "app", vec![a, b, c]);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].target, "/target/a");
        assert_eq!(result[1].target, "/target/b");
        assert_eq!(result[2].target, "/target/c");
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
    // Pass-1 exact-match regression tests (Finding P2)
    // -----------------------------------------------------------------------

    #[test]
    fn pass_one_preserves_ssh_socket_under_base_tmp_bind() {
        // User's base compose has /tmp:/tmp. Cella's env_fwd has SSH socket
        // at /tmp/cella-ssh-agent.sock (a descendant). Socket must survive
        // because pass-1 only drops exact-target matches.
        let resolved = make_resolved(
            "app",
            vec![json!({"type": "bind", "source": "/tmp", "target": "/tmp"})],
        );
        let candidates = vec![MountSpec::bind(
            "/tmp/cella-ssh-agent.sock",
            "/tmp/cella-ssh-agent.sock",
        )];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert_eq!(
            result.len(),
            1,
            "SSH socket descendant of base /tmp must survive; got: {result:?}"
        );
        assert_eq!(result[0].target, "/tmp/cella-ssh-agent.sock");
    }

    #[test]
    fn pass_one_preserves_feature_subdir_under_user_parent_mount() {
        // Feature mounts /root/.config/feature-x. User's base has /root/.config.
        // Feature subdir must survive — it's an intentional overlay.
        let resolved = make_resolved(
            "app",
            vec![json!({"type": "bind", "source": "/host/config", "target": "/root/.config"})],
        );
        let candidates = vec![MountSpec::bind(
            "/host/feature-x-data",
            "/root/.config/feature-x",
        )];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert_eq!(
            result.len(),
            1,
            "feature subdir under user-owned parent must survive; got: {result:?}"
        );
        assert_eq!(result[0].target, "/root/.config/feature-x");
    }

    #[test]
    fn pass_one_still_drops_exact_target_match() {
        // User explicitly mounted /root/.claude — cella's /root/.claude bind dropped.
        let resolved = make_resolved(
            "app",
            vec![json!({"type": "bind", "source": "/user/.claude", "target": "/root/.claude"})],
        );
        let candidates = vec![MountSpec::bind("/cella/.claude", "/root/.claude")];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert!(
            result.is_empty(),
            "cella's exact-target bind must be dropped in favor of user's; got: {result:?}"
        );
    }

    #[test]
    fn pass_one_exact_match_with_trailing_slash_in_base() {
        // Base target has a trailing slash — after normalisation it must still
        // drop a candidate at the same path (no trailing slash).
        let resolved = make_resolved(
            "app",
            vec![json!({"type": "bind", "source": "/h", "target": "/root/.claude/"})],
        );
        let candidates = vec![MountSpec::bind("/foo", "/root/.claude")];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert!(
            result.is_empty(),
            "trailing slash on base target must not defeat exact-match dropping; got: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Trailing-slash normalization on candidate side (Finding P2 — round 5)
    // -----------------------------------------------------------------------

    #[test]
    fn dedup_normalizes_candidate_trailing_slash_against_base() {
        // Base: /root/.claude (no slash). Candidate: /root/.claude/ (with slash).
        // The candidate must be dropped because the normalised forms are equal.
        let resolved = make_resolved(
            "app",
            vec![json!({"type": "bind", "source": "/x", "target": "/root/.claude"})],
        );
        let candidates = vec![MountSpec::bind("/cella/x", "/root/.claude/")];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert!(
            result.is_empty(),
            "candidate with trailing slash should match base without; got: {result:?}"
        );
    }

    #[test]
    fn dedup_normalizes_pass_two_trailing_slash_collision() {
        // Pass 2: two cella candidates targeting the same logical path, one with
        // a trailing slash. Last-wins: the second candidate (with slash) survives.
        let resolved = make_resolved("app", vec![]);
        let candidates = vec![
            MountSpec::bind("/a", "/root/.foo"),
            MountSpec::bind("/b", "/root/.foo/"),
        ];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert_eq!(
            result.len(),
            1,
            "trailing-slash variant must dedup against bare; got: {result:?}"
        );
        // Last-wins: the second candidate survives.
        assert_eq!(result[0].source, "/b");
    }

    #[test]
    fn dedup_preserves_root_target_as_slash() {
        // Edge case: candidate target is "/" (root). normalize_target must not
        // collapse it to an empty string, leaving the root mount intact.
        let resolved = make_resolved("app", vec![]);
        let candidates = vec![MountSpec::bind("/host", "/")];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert_eq!(
            result.len(),
            1,
            "root-target mount must survive; got: {result:?}"
        );
        assert_eq!(result[0].target, "/");
    }

    // -----------------------------------------------------------------------
    // Compose mount precedence: auto-forwarded wins over user on collision
    // (Finding P2 round 5 — assembly order in build_compose_mount_specs)
    // -----------------------------------------------------------------------

    #[test]
    fn dedup_auto_forwarded_mount_wins_over_user_mount_on_collision() {
        // build_compose_mount_specs places user/feature mounts FIRST and
        // auto-forwarded (tool-config, env-fwd) mounts LAST. With last-wins
        // dedup, the auto-forwarded mount survives on an exact-target collision —
        // matching single-container behaviour.
        //
        // This test exercises dedup_against_base directly with the same ordering
        // that build_compose_mount_specs produces; it validates the last-wins
        // invariant that the reorder relies on.
        let resolved = make_resolved("app", vec![]);
        let candidates = vec![
            // User mount (first — as map_merged_mounts returns it)
            MountSpec::bind("/user/.claude", "/root/.claude"),
            // Auto-forwarded tool-config mount (last — appended after user mounts)
            MountSpec::bind("/host/.claude", "/root/.claude"),
        ];
        let result = dedup_against_base(&resolved, "app", candidates);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].source, "/host/.claude",
            "auto-forwarded mount must win over user mount on exact target collision"
        );
    }

    // -----------------------------------------------------------------------
    // filter_reserved_agent (Finding 1)
    // -----------------------------------------------------------------------

    #[test]
    fn reject_mount_targeting_cella_root() {
        // A user mount targeting `/cella` exactly must be filtered out.
        let specs = vec![MountSpec::bind("/host-cella", "/cella")];
        let result = filter_reserved_agent(specs, "/cella", "cella-agent");
        assert!(
            result.is_empty(),
            "mount at /cella must be rejected; got: {result:?}"
        );
    }

    #[test]
    fn reject_mount_targeting_cella_descendant() {
        // A user mount targeting `/cella/bin` (descendant) must be filtered out.
        let specs = vec![MountSpec::bind("/host-bin", "/cella/bin")];
        let result = filter_reserved_agent(specs, "/cella", "cella-agent");
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
        let result = filter_reserved_agent(specs, "/cella", "cella-agent");
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
        let result = filter_reserved_agent(specs, "/cella", "cella-agent");
        assert_eq!(
            result.len(),
            1,
            "only the /workspace mount should survive; got: {result:?}"
        );
        assert_eq!(result[0].target, "/workspace");
    }

    #[test]
    fn reject_mount_aliasing_agent_volume_by_source() {
        // A volume mount sourced from the agent volume name must be rejected even
        // when its target is outside the reserved `/cella` subtree.
        let specs = vec![MountSpec {
            kind: MountKind::Volume,
            source: "cella-agent".to_string(),
            target: "/tmp/agent-rw".to_string(),
            read_only: false,
            consistency: None,
        }];
        let result = filter_reserved_agent(specs, "/cella", "cella-agent");
        assert!(
            result.is_empty(),
            "volume aliasing agent volume by source must be rejected; got: {result:?}"
        );
    }

    #[test]
    fn allow_non_volume_mount_with_name_matching_agent() {
        // A bind mount whose source string matches the agent volume name is NOT
        // rejected — bind mounts don't share volume identity.
        let specs = vec![MountSpec {
            kind: MountKind::Bind,
            source: "cella-agent".to_string(),
            target: "/whatever".to_string(),
            read_only: false,
            consistency: None,
        }];
        let result = filter_reserved_agent(specs, "/cella", "cella-agent");
        assert_eq!(
            result.len(),
            1,
            "bind mount with agent-named source must be kept; got: {result:?}"
        );
    }

    #[test]
    fn allow_volume_mount_with_different_source() {
        // A volume mount with a different source name must pass through.
        let specs = vec![MountSpec {
            kind: MountKind::Volume,
            source: "mycache".to_string(),
            target: "/cache".to_string(),
            read_only: false,
            consistency: None,
        }];
        let result = filter_reserved_agent(specs, "/cella", "cella-agent");
        assert_eq!(
            result.len(),
            1,
            "volume with non-agent source must be kept; got: {result:?}"
        );
    }
}
