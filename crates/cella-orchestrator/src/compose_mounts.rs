//! Mount-list utilities for the compose `up` path.
//!
//! Provides target-path dedup against resolved compose config and adapters
//! for converting `EnvForwarding` and `MountConfig` inputs into `MountSpec`.
//! Mount assembly (combining all sources) lives in `compose_up.rs`.

use std::collections::HashSet;
use std::path::Path;

use cella_backend::{MountConfig, MountKind, MountSpec};
use cella_compose::config::ResolvedComposeConfig;
use cella_env::EnvForwarding;
use sha2::{Digest, Sha256};

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

/// Extract service names from a `depends_on` value (short or long form).
///
/// - Array of strings → each element is a dependency name.
/// - Object keyed by service name → each key is a dependency name.
/// - Null / absent → no dependencies.
fn depends_on_names(value: &serde_json::Value) -> Vec<&str> {
    match value {
        serde_json::Value::Array(arr) => arr.iter().filter_map(|v| v.as_str()).collect(),
        serde_json::Value::Object(obj) => obj.keys().map(String::as_str).collect(),
        _ => Vec::new(),
    }
}

/// Compute the set of services compose will start given an optional
/// `run_services` filter.
///
/// - `None` → compose starts all services → the full service map is the closure.
/// - `Some(list)` → compose starts the listed services plus their transitive
///   `depends_on` closure (compose always follows dependencies unless
///   `--no-deps` is given; cella never passes `--no-deps`).
fn compute_service_closure<'a>(
    resolved: &'a ResolvedComposeConfig,
    run_services: Option<&'a [String]>,
) -> HashSet<&'a str> {
    let mut closure: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = run_services.map_or_else(
        || resolved.services.keys().map(String::as_str).collect(),
        |list| list.iter().map(String::as_str).collect(),
    );
    while let Some(name) = stack.pop() {
        if !closure.insert(name) {
            continue;
        }
        if let Some(svc) = resolved.services.get(name) {
            for dep in depends_on_names(&svc.depends_on) {
                if !closure.contains(dep) {
                    stack.push(dep);
                }
            }
        }
    }
    closure
}

/// Validate that the user's base compose config does not alias the managed
/// agent volume in any service that compose will actually start.
///
/// Four aliasing patterns are checked across services in the computed closure:
///
/// 1. **Source alias** — a service volume entry of type `volume` whose
///    `source` matches `agent_vol_name` exactly. Docker would mount the same
///    underlying volume at a second path, making the agent volume writable via
///    that alias. Bind mounts with a source string matching the agent volume
///    name are **not** rejected: they point at a host directory, not the
///    Docker volume.
///
/// 2. **Target subtree alias** — a service volume entry whose `target` is
///    equal to or a descendant of `agent_vol_target` (e.g., `/cella`). This
///    would shadow or overwrite the agent at that path.
///    Applies to the **primary service only**: cella injects the agent volume
///    into the primary; sidecars run in their own container filesystem so a
///    sidecar mount at `/cella/foo` cannot shadow the primary's agent path.
///
/// 3. **Top-level volume name alias** — any top-level volume entry that has a
///    `name:` field equal to `agent_vol_name`. If a service references that
///    compose-key by source, it silently mounts the agent volume even though
///    the source string looks innocuous. Only applies when the mount type is
///    `volume`.
///
/// 4. **Writable `volumes_from` on primary** — inheriting volumes from the
///    primary service in writable mode (no `:ro` suffix, or `read_only: true`
///    not set) would bring in cella's injected agent mount without cella's
///    read-only protection. Read-only inheritance (`svc:ro` or
///    `read_only: true`) is safe and allowed.
///
/// Only services in the `run_services` + transitive `depends_on` closure are
/// inspected. When `run_services` is `None` (no filter — compose starts
/// everything), all services are validated. Unrelated utility services not
/// reachable from the explicit service list are skipped.
///
/// Returns `Ok(())` when the config is clean, or `Err(message)` with a
/// human-readable description of what was rejected.
///
/// # Notes
///
/// Both long-form object entries and short-form string entries (e.g.
/// `"host:/container"`) are handled. Short-form strings are parsed by
/// [`parse_short_form_volume`] to extract `source`, `target`, and inferred
/// mount type. This is necessary because some `docker compose config --format
/// json` versions still emit short-form strings for ordinary bind mounts.
///
/// This function must be called with a config resolved via
/// `ComposeCommand::without_override` so that cella's own injected agent mount
/// does not trigger a false-positive self-rejection.
pub(crate) fn validate_base_compose_against_reserved_agent(
    resolved: &ResolvedComposeConfig,
    agent_vol_name: &str,
    agent_vol_target: &str,
    primary_service: &str,
    run_services: Option<&[String]>,
) -> Result<(), String> {
    // Build the set of compose volume keys whose `name` field resolves to the
    // agent volume name.  E.g. `pretty-name: { name: cella-agent }` makes the
    // key "pretty-name" an alias even though its compose-side key is different.
    let aliased_keys: HashSet<&str> = resolved
        .volumes
        .iter()
        .filter_map(|(key, val)| {
            val.get("name")
                .and_then(|n| n.as_str())
                .filter(|name| *name == agent_vol_name)
                .map(|_| key.as_str())
        })
        .collect();

    // Check 0.5: reject if the base compose file declares the agent volume key
    // itself (`cella-agent: { ... }`) with any attribute other than a matching
    // `name:`.  Compose deep-merges top-level volume maps, so user attributes on
    // the `cella-agent` key survive into the merged output even when cella's
    // override also declares `cella-agent: { external: true, name: cella-agent }`.
    // A user-supplied `name: attacker-vol` or `driver: nfs` would silently win
    // or produce an inconsistency error (external+conflicting-name), breaking the
    // trust boundary.
    if let Some(agent_base) = resolved.volumes.get(agent_vol_name)
        && !is_compatible_base_volume_declaration(agent_base, agent_vol_name)
    {
        return Err(format!(
            "base compose top-level volume '{agent_vol_name}' has cella-incompatible \
             attributes (cella manages this volume's identity; base declarations must be \
             empty or `name: {agent_vol_name}` only). Remove the declaration or clear \
             its fields.",
        ));
    }

    // Determine the set of services compose will actually start.
    let closure = compute_service_closure(resolved, run_services);

    for (svc_name, svc) in &resolved.services {
        // Skip services outside the run_services + depends_on closure.
        if !closure.contains(svc_name.as_str()) {
            continue;
        }

        // Check 0: volumes_from inherits ALL volumes from the named service at
        // runtime. Only writable inheritance from the primary service bypasses
        // cella's read-only agent protection.
        check_volumes_from(svc_name, svc, primary_service)?;

        for entry in &svc.volumes {
            let (mount_type, source, target) = match entry {
                serde_json::Value::Object(obj) => {
                    let mt = obj
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("volume");
                    let src = obj.get("source").and_then(|s| s.as_str()).unwrap_or("");
                    let tgt = obj.get("target").and_then(|t| t.as_str()).unwrap_or("");
                    (mt, src, tgt)
                }
                serde_json::Value::String(s) => {
                    // Short-form: parse source and target, infer mount type.
                    // A source with no leading '/' or '.' is a named volume;
                    // a source with '/' or '.' prefix is a bind path; empty source
                    // (anonymous form "/path") is an anonymous volume.
                    let Some((src_opt, tgt)) = parse_short_form_volume(s) else {
                        continue; // malformed — skip
                    };
                    let src = src_opt.unwrap_or("");
                    let mt = if src.starts_with('/') || src.starts_with('.') || src.is_empty() {
                        "bind"
                    } else {
                        "volume"
                    };
                    (mt, src, tgt)
                }
                _ => continue, // not object or string — not spec-compliant; ignore
            };

            // Checks 1 and 2 only apply to volume-type mounts.
            // Bind mounts with source matching the agent volume name point at a
            // host directory, not the Docker volume — not an alias.
            if mount_type == "volume" {
                // Check 1: source matches agent volume name directly.
                if source == agent_vol_name {
                    return Err(format!(
                        "compose file service '{svc_name}' mounts or aliases the managed \
                         agent volume (source='{agent_vol_name}' or target inside \
                         '{agent_vol_target}'): rejected source='{source}' target='{target}'"
                    ));
                }

                // Check 2: source is a top-level volume key aliased to the agent volume name.
                if aliased_keys.contains(source) {
                    return Err(format!(
                        "compose file service '{svc_name}' mounts or aliases the managed \
                         agent volume (source='{agent_vol_name}' or target inside \
                         '{agent_vol_target}'): rejected source='{source}' (top-level volume \
                         name aliases '{agent_vol_name}') target='{target}'"
                    ));
                }
            }

            // Check 3: target is inside the reserved agent subtree.
            //
            // The override injects the agent volume into the primary service
            // only. Sidecars run in their own container filesystem: a sidecar
            // mounting something at `/cella/foo` in its own namespace cannot
            // shadow the primary's agent path. Only enforce this check for
            // the primary service.
            if svc_name == primary_service
                && !target.is_empty()
                && is_descendant_or_equal(target, agent_vol_target)
            {
                return Err(format!(
                    "compose file service '{svc_name}' mounts or aliases the managed \
                     agent volume (source='{agent_vol_name}' or target inside '{agent_vol_target}'): \
                     rejected source='{source}' target='{target}'"
                ));
            }
        }
    }

    Ok(())
}

/// Return `true` if a base compose top-level volume declaration is compatible
/// with cella pinning that volume to the literal name `expected_name`.
///
/// Compatible means the merged declaration will not carry attributes that alter
/// volume identity or impose external pre-existence requirements:
///
/// - `null` / JSON null → treated as bare (compatible).
/// - `{}` (empty object) → bare key, no conflicting attributes (compatible).
/// - `{ name: <expected_name> }` → explicit name matches literal (compatible).
/// - Anything else — `external`, `driver`, `driver_opts`, `labels`, a `name`
///   that differs from `expected_name` — is **incompatible**.
///
/// The function is called for both the agent volume key and user-declared extra
/// volume keys.
fn is_compatible_base_volume_declaration(value: &serde_json::Value, expected_name: &str) -> bool {
    let Some(obj) = value.as_object() else {
        // null or non-object primitive: treat as bare
        return value.is_null();
    };
    if obj.is_empty() {
        return true; // bare {}
    }
    for (k, v) in obj {
        if k == "name" {
            if v.as_str() != Some(expected_name) {
                return false; // name mismatch
            }
        } else {
            // Any non-name attribute (external, driver, driver_opts, labels…)
            return false;
        }
    }
    true
}

/// Reject writable `volumes_from` entries that inherit from the primary service,
/// or that use the `container:<name>` form in writable mode.
///
/// - Writable inheritance from the primary service would bring in cella's
///   injected agent mount without cella's read-only protection.
/// - Writable `container:<name>` entries inherit volumes from an arbitrary
///   running container (potentially outside the project) and could expose the
///   managed agent volume at read-write. Read-only inheritance preserves
///   protection and is safe.
///
/// Read-only inheritance (`:ro` suffix / `read_only: true`) is always allowed.
fn check_volumes_from(
    svc_name: &str,
    svc: &cella_compose::config::ResolvedService,
    primary_service: &str,
) -> Result<(), String> {
    for vf_entry in &svc.volumes_from {
        let (inherit_from, read_only, is_container) = parse_volumes_from_entry(vf_entry);
        // container: form check — must come before primary-service check so that
        // `container:app` (where `app` is the primary) gets the container-form
        // error rather than the primary-inheritance one.
        if !read_only && is_container {
            return Err(format!(
                "service '{svc_name}' uses writable volumes_from with container:<name> form. \
                 This form inherits volumes from a running container (possibly outside the \
                 project) and could expose the managed agent volume at read-write. Use the \
                 service-name form with ':ro' suffix if you need read-only inheritance, or \
                 remove the volumes_from entry."
            ));
        }
        if inherit_from == primary_service && !read_only {
            return Err(format!(
                "service '{svc_name}' uses writable volumes_from on primary service \
                 '{primary_service}', which would inherit cella's managed agent volume \
                 without cella's read-only protection. Use ':ro' suffix or \
                 'read_only: true' for read-only inheritance, or remove the \
                 volumes_from entry."
            ));
        }
    }
    Ok(())
}

/// Extract (`service_name_or_container`, `is_read_only`, `is_container_form`)
/// from a single `volumes_from` entry.
///
/// String form examples:
/// - `"app"` → `("app", false, false)`
/// - `"app:ro"` → `("app", true, false)`
/// - `"container:primary"` → `("primary", false, true)`
/// - `"container:primary:ro"` → `("primary", true, true)`
///
/// Object form: `source`/`from` key is inspected for a `container:` prefix.
fn parse_volumes_from_entry(entry: &serde_json::Value) -> (&str, bool, bool) {
    match entry {
        serde_json::Value::String(s) => {
            let parts: Vec<&str> = s.rsplitn(2, ':').collect();
            let (rest, is_ro) = if parts.len() == 2 && (parts[0] == "ro" || parts[0] == "rw") {
                (parts[1], parts[0] == "ro")
            } else {
                (s.as_str(), false)
            };
            // Check container: prefix on the remaining portion.
            rest.strip_prefix("container:")
                .map_or((rest, is_ro, false), |stripped| (stripped, is_ro, true))
        }
        serde_json::Value::Object(obj) => {
            let name = obj
                .get("source")
                .or_else(|| obj.get("from"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ro = obj
                .get("read_only")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            // Object form may also use "container:" prefix in source.
            name.strip_prefix("container:")
                .map_or((name, ro, false), |stripped| (stripped, ro, true))
        }
        _ => ("", false, false),
    }
}

/// Reject if any extra named-volume source collides with an existing base
/// top-level volume key that has cella-incompatible attributes, or would
/// retarget a project-scoped named volume already used by a base service.
///
/// Docker Compose deep-merges top-level volume declarations across files.
/// Once cella's override references a source as a named volume, the base key
/// is live at runtime regardless of whether any *base* service referenced it.
/// Any attributes the base key carries will survive the merge and stick to the
/// final declaration, potentially:
///
/// - Requiring the volume to pre-exist (`external: true`).
/// - Changing the backing driver or its options (`driver`, `driver_opts`).
/// - Overriding volume identity (`name` pointing at a different literal).
///
/// An additional retargeting check applies when there is **no** top-level
/// entry (or it does not already pin `name: <source>`): if any base service
/// already references `source` as a named volume mount, cella's literal-name
/// pin would change that service's volume from `<project>_<source>` to the
/// global `<source>`, effectively forking its data. In that case the pin is
/// rejected unless the base explicitly opts in by declaring
/// `volumes.<source>.name: <source>`.
///
/// The rules applied per extra volume `source`:
///
/// - No top-level base entry, no service reference → no collision, emit normally.
/// - `{}` (bare key) → compatible: cella's `name: <source>` pin fully defines
///   identity; no base attribute survives to conflict. Service references are safe.
/// - `{ name: <source> }` → already pinned; cella's emission is idempotent.
/// - Anything else (`external`, `driver`, `driver_opts`, `labels`, mismatched
///   `name`) → **reject** (top-level compatibility check).
/// - No top-level entry but a service uses `source` as a volume mount → reject
///   (retarget check); user must add `volumes.<source>.name: <source>` to opt in.
///
/// Returns `Ok(())` when all extra volumes are safe, or `Err(message)` on the
/// first incompatible or retargeting declaration.
pub(crate) fn validate_extra_named_volumes_against_base(
    resolved: &ResolvedComposeConfig,
    extra_volumes: &[MountSpec],
) -> Result<(), String> {
    for spec in extra_volumes {
        if spec.kind != MountKind::Volume || spec.source.is_empty() {
            continue;
        }

        let top_level = resolved.volumes.get(&spec.source);

        // Check 1: top-level compatibility (existing check — driver, external, etc.)
        let top_level_compatible =
            top_level.is_none_or(|v| is_compatible_base_volume_declaration(v, &spec.source));
        if !top_level_compatible {
            return Err(format!(
                "devcontainer mount source '{}' collides with base compose top-level \
                 volume declaration that has cella-incompatible attributes. Rename the \
                 mount, or clear the base top-level declaration's attributes (leave only \
                 `name: {}` if needed).",
                spec.source, spec.source,
            ));
        }

        // Check 2: retarget check — only needed when the top-level does NOT already
        // pin the literal name. If it does, the base service already resolves to the
        // literal volume, so cella's emission is idempotent and safe.
        let top_level_pins_name = top_level
            .and_then(serde_json::Value::as_object)
            .and_then(|obj| obj.get("name"))
            .and_then(serde_json::Value::as_str)
            == Some(spec.source.as_str());

        if !top_level_pins_name {
            check_extra_volume_against_base_services(resolved, &spec.source)?;
        }
    }
    Ok(())
}

/// Scan base service volumes for a named-volume mount whose source matches
/// `source`. Returns `Err` if found — cella's literal-name pin would retarget
/// the service's `<project>_<source>` volume to the global `<source>` volume.
///
/// Extracted from [`validate_extra_named_volumes_against_base`] to stay within
/// clippy's `too_many_lines` limit.
fn check_extra_volume_against_base_services(
    resolved: &ResolvedComposeConfig,
    source: &str,
) -> Result<(), String> {
    for (svc_name, svc) in &resolved.services {
        for entry in &svc.volumes {
            let (mount_type, src) = match entry {
                serde_json::Value::Object(obj) => {
                    let mt = obj
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("volume");
                    let s = obj
                        .get("source")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    (mt, s)
                }
                serde_json::Value::String(s) => {
                    let Some((src_opt, _)) = parse_short_form_volume(s) else {
                        continue;
                    };
                    let src_val = src_opt.unwrap_or("");
                    // A source with a leading '/' or '.' is a bind path; an empty
                    // source is an anonymous volume — neither is a named-volume ref.
                    let mt = if src_val.starts_with('/')
                        || src_val.starts_with('.')
                        || src_val.is_empty()
                    {
                        "bind"
                    } else {
                        "volume"
                    };
                    (mt, src_val)
                }
                _ => continue,
            };
            if mount_type == "volume" && src == source {
                return Err(format!(
                    "devcontainer mount source '{source}' is already used as a named \
                     volume by base service '{svc_name}'. Cella cannot safely pin the \
                     literal Docker volume name without retargeting that service (from \
                     <project>_{source} to global {source}). Rename the mount, or \
                     explicitly add a compatible top-level declaration \
                     (volumes.{source}.name = {source}) to opt into the literal pin.",
                ));
            }
        }
    }
    Ok(())
}

/// Parse a short-form compose volume string into `(source, target)`.
///
/// Forms:
/// - `"/path"` → `(None, "/path")` — anonymous volume or bare target
/// - `"name:/path"` → `(Some("name"), "/path")` — named volume or bind
/// - `"host:/container:opts"` → `(Some("host"), "/container")` — bind with opts
///
/// Returns `None` for malformed input (empty string or empty target path).
///
/// # Note
///
/// Windows drive-letter paths such as `C:\foo:/bar` are **not** supported on
/// the Linux-first cella path and will be parsed ambiguously (the drive letter
/// becomes the "source", the rest becomes the "target"). Cella is Linux-first
/// per its architecture documentation; Windows paths should not appear in
/// practice.
fn parse_short_form_volume(s: &str) -> Option<(Option<&str>, &str)> {
    let parts: Vec<&str> = s.splitn(3, ':').collect();
    match parts.len() {
        1 => {
            // Anonymous volume or bare target (no colon).
            if parts[0].is_empty() {
                None
            } else {
                Some((None, parts[0]))
            }
        }
        _ => {
            // Has at least one colon — source:target[:opts].
            if parts[1].is_empty() {
                None
            } else {
                Some((Some(parts[0]), parts[1]))
            }
        }
    }
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
///
/// # Errors
///
/// `extract_service_targets` is infallible; this function propagates its
/// `Result` for the caller's convenience and currently always returns `Ok`.
pub fn dedup_against_base(
    resolved: &ResolvedComposeConfig,
    primary_service: &str,
    candidates: Vec<MountSpec>,
) -> Result<Vec<MountSpec>, String> {
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
    Ok(after_base)
}

/// Extract the set of normalised mount target paths declared for `service` in
/// the resolved compose config.
///
/// Both long-form object entries (with a `"target"` key) and short-form string
/// entries (e.g. `"host:/container"` or `"/path"`) are handled. Short-form
/// strings are parsed by [`parse_short_form_volume`] to extract the target.
/// Malformed entries (empty string, empty target) are silently skipped.
/// Non-object, non-string entries are also silently skipped.
///
/// `docker compose config --format json` normalises volume entries to long-form
/// objects on most Compose versions, but some versions still emit short-form
/// strings for ordinary bind mounts. Both forms receive the same dedup checks.
fn extract_service_targets(resolved: &ResolvedComposeConfig, service: &str) -> HashSet<String> {
    let Some(svc) = resolved.services.get(service) else {
        return HashSet::new();
    };
    let mut out = HashSet::new();
    for entry in &svc.volumes {
        match entry {
            serde_json::Value::Object(obj) => {
                if let Some(t) = obj.get("target").and_then(serde_json::Value::as_str) {
                    // Normalise so that "/root/.claude/" and "/root/.claude" compare
                    // equal against candidate targets, and "/" is never collapsed to "".
                    out.insert(normalize_target(t).to_string());
                }
            }
            serde_json::Value::String(s) => {
                if let Some((_src, target)) = parse_short_form_volume(s) {
                    out.insert(normalize_target(target).to_string());
                }
                // else: malformed entry — silently skip.
            }
            _ => {
                // Neither object nor string — not spec-compliant; ignore.
            }
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

/// Compute a stable fingerprint over mount-affecting inputs that are **not**
/// reflected in `project.config_hash` (which only covers devcontainer.json +
/// compose file contents).
///
/// Hashes the tool `forward_config` flags, tool config override paths, the
/// resolved host paths for each enabled tool (so installing/removing a tool
/// config directory flips the fingerprint even without changing settings),
/// the env-forwarding mount list, and the presence/path of a parent git
/// directory.
///
/// This fingerprint is stored as `dev.cella.mount_input_fingerprint` at
/// container creation time and recomputed on reconnect to detect drift in
/// settings, SSH/GPG agent state, or git worktree layout.
///
/// The hash is order-dependent and deterministic: same inputs always produce
/// the same hex string.
pub fn compute_mount_input_fingerprint(
    settings: &cella_config::settings::Settings,
    env_fwd: &EnvForwarding,
    workspace_root: &Path,
) -> String {
    let mut hasher = Sha256::new();
    let t = &settings.tools;

    // Tool forward_config booleans.
    hasher.update([u8::from(t.claude_code.forward_config)]);
    hasher.update([u8::from(t.codex.forward_config)]);
    hasher.update([u8::from(t.gemini.forward_config)]);
    hasher.update([u8::from(t.nvim.forward_config)]);
    hasher.update([u8::from(t.tmux.forward_config)]);

    // Tool config override paths (None is represented by a bare NUL separator).
    for path in [t.nvim.config_path.as_deref(), t.tmux.config_path.as_deref()] {
        if let Some(p) = path {
            hasher.update(p.as_bytes());
        }
        hasher.update(b"\0");
    }

    // Tool host-path detection results.
    //
    // Include the resolved host path for each enabled tool so that installing
    // or removing a tool config (e.g. adding ~/.codex after an initial `cella
    // up`) flips the fingerprint and triggers a drift warning on reconnect.
    // The paths checked here must match exactly what `build_tool_config_mount_specs`
    // consults so that any change to the actual mount set is reflected.
    hash_tool_host_paths(&mut hasher, settings);

    // env_fwd mount list (SSH socket, GPG agent, etc.).
    for m in &env_fwd.mounts {
        hasher.update(m.source.as_bytes());
        hasher.update(b"|");
        hasher.update(m.target.as_bytes());
        hasher.update(b"\n");
    }

    // Parent git directory presence + canonical path.
    //
    // Canonicalize mirrors the same canonicalize+fallback applied in
    // `build_compose_mount_specs` when assembling the actual parent-git mount.
    // Without this, linked worktrees with symlinked .gitdir pointers would
    // produce a fingerprint that describes a different path than what is
    // actually mounted, causing false drift detections on reconnect.
    if let Some(pg) = cella_git::parent_git_dir(workspace_root) {
        let canonical = pg.canonicalize().unwrap_or_else(|_| pg.clone());
        hasher.update(b"pg:");
        hasher.update(canonical.to_string_lossy().as_bytes());
    }

    hex::encode(hasher.finalize())
}

/// Hash the resolved host paths for all enabled tool configs into `hasher`.
///
/// Called from [`compute_mount_input_fingerprint`]. Extracted into a helper to
/// keep that function within clippy's `too_many_lines` limit.
fn hash_tool_host_paths(hasher: &mut Sha256, settings: &cella_config::settings::Settings) {
    let t = &settings.tools;

    if t.claude_code.forward_config {
        if let Some(p) = cella_env::claude_code::host_claude_json_path() {
            hasher.update(b"claude_json:");
            hasher.update(p.to_string_lossy().as_bytes());
            hasher.update(b"\0");
        }
        if let Some(p) = cella_env::claude_code::host_claude_dir() {
            hasher.update(b"claude_dir:");
            hasher.update(p.to_string_lossy().as_bytes());
            hasher.update(b"\0");
        }
        if let Some(p) = cella_env::claude_code::host_plugins_dir() {
            hasher.update(b"claude_plugins:");
            hasher.update(p.to_string_lossy().as_bytes());
            hasher.update(b"\0");
        }
    }
    if t.codex.forward_config
        && let Some(p) = cella_env::codex::host_codex_dir()
    {
        hasher.update(b"codex_dir:");
        hasher.update(p.to_string_lossy().as_bytes());
        hasher.update(b"\0");
    }
    if t.gemini.forward_config
        && let Some(p) = cella_env::gemini::host_gemini_dir()
    {
        hasher.update(b"gemini_dir:");
        hasher.update(p.to_string_lossy().as_bytes());
        hasher.update(b"\0");
    }
    if t.nvim.forward_config
        && let Some(p) = cella_env::nvim::host_nvim_config_dir(t.nvim.config_path.as_deref())
    {
        hasher.update(b"nvim_dir:");
        hasher.update(p.to_string_lossy().as_bytes());
        hasher.update(b"\0");
    }
    if t.tmux.forward_config {
        if let Some(p) = cella_env::tmux::host_tmux_conf(t.tmux.config_path.as_deref()) {
            hasher.update(b"tmux_conf:");
            hasher.update(p.to_string_lossy().as_bytes());
            hasher.update(b"\0");
        }
        if let Some(p) = cella_env::tmux::host_tmux_config_dir(t.tmux.config_path.as_deref()) {
            hasher.update(b"tmux_dir:");
            hasher.update(p.to_string_lossy().as_bytes());
            hasher.update(b"\0");
        }
    }
}

/// Resolve relative bind-source paths against a base directory.
///
/// Docker Compose resolves relative `source:` paths relative to the compose
/// file's directory. Cella writes its override to `~/.cella/compose/<project>/`,
/// so any relative source in a user/feature mount would resolve to that
/// internal directory instead of the user's workspace. This function
/// canonicalizes relative bind sources against `workspace_root` before emission.
///
/// - Absolute sources and empty sources (tmpfs) are left unchanged.
/// - Non-bind kinds (Volume, Tmpfs, `NamedPipe`) are left unchanged.
/// - If `canonicalize` fails (e.g. the path does not yet exist), the raw join
///   is used as-is so the user sees a compose error rather than a silent wrong path.
pub(crate) fn resolve_bind_sources(specs: &mut [MountSpec], workspace_root: &Path) {
    for spec in specs.iter_mut() {
        if spec.kind != MountKind::Bind {
            continue;
        }
        let source_path = Path::new(&spec.source);
        if source_path.is_absolute() || spec.source.is_empty() {
            continue;
        }
        // Relative — resolve against workspace_root and canonicalize if possible.
        let joined = workspace_root.join(source_path);
        spec.source = joined
            .canonicalize()
            .unwrap_or(joined)
            .to_string_lossy()
            .into_owned();
    }
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
                volumes_from: vec![],
                depends_on: serde_json::Value::default(),
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
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].target, "/root/.codex");
    }

    #[test]
    fn extract_service_targets_parses_short_form_strings() {
        // Some `docker compose config --format json` versions emit short-form
        // strings for ordinary bind mounts. These must be parsed, not hard-failed.
        let resolved = make_resolved(
            "app",
            vec![
                json!("/anonymous"),        // anonymous: target = /anonymous
                json!("named:/data"),       // named volume or bind: target = /data
                json!("./cache:/cache:ro"), // bind with opts: target = /cache
            ],
        );
        let candidates = vec![
            MountSpec::bind("/x", "/anonymous"), // collides with entry 1 → dropped
            MountSpec::bind("/y", "/data"),      // collides with entry 2 → dropped
            MountSpec::bind("/z", "/cache"),     // collides with entry 3 → dropped
            MountSpec::bind("/w", "/other"),     // no collision → kept
        ];
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].target, "/other");
    }

    #[test]
    fn dedup_unknown_service_returns_all_candidates() {
        let resolved = make_resolved(
            "app",
            vec![json!({"type": "bind", "source": "/x", "target": "/root/.claude"})],
        );
        let candidates = vec![MountSpec::bind("/cella/claude", "/root/.claude")];
        // "web" is not in resolved config — all candidates must pass through
        let result = dedup_against_base(&resolved, "web", candidates).unwrap();
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
                volumes_from: vec![],
                depends_on: serde_json::Value::default(),
            },
        );
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![],
                depends_on: serde_json::Value::default(),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            ..Default::default()
        };
        let candidates = vec![MountSpec::bind("/x", "/root/.claude")];
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
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
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
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
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
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
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
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
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
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
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
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
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
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
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
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
        let result = dedup_against_base(&resolved, "app", vec![first, last]).unwrap();
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
        let result = dedup_against_base(&resolved, "app", vec![a, b, c]).unwrap();
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
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
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
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
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
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
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
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
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
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
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
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
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
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
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
        let result = dedup_against_base(&resolved, "app", candidates).unwrap();
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

    // -----------------------------------------------------------------------
    // compute_mount_input_fingerprint
    // -----------------------------------------------------------------------

    #[test]
    fn mount_input_fingerprint_stable_across_calls() {
        let settings = cella_config::settings::Settings::default();
        let env_fwd = EnvForwarding::default();
        let ws = Path::new("/tmp/nowhere-should-not-exist-cella-xyz");
        let fp1 = compute_mount_input_fingerprint(&settings, &env_fwd, ws);
        let fp2 = compute_mount_input_fingerprint(&settings, &env_fwd, ws);
        assert_eq!(fp1, fp2, "fingerprint must be deterministic");
    }

    #[test]
    fn mount_input_fingerprint_changes_on_forward_config_toggle() {
        let mut settings = cella_config::settings::Settings::default();
        let env_fwd = EnvForwarding::default();
        let ws = Path::new("/tmp/nowhere-should-not-exist-cella-xyz");
        let fp_before = compute_mount_input_fingerprint(&settings, &env_fwd, ws);
        settings.tools.claude_code.forward_config = !settings.tools.claude_code.forward_config;
        let fp_after = compute_mount_input_fingerprint(&settings, &env_fwd, ws);
        assert_ne!(
            fp_before, fp_after,
            "fingerprint must change when claude_code.forward_config is toggled"
        );
    }

    #[test]
    fn mount_input_fingerprint_changes_on_env_fwd_mount_change() {
        let settings = cella_config::settings::Settings::default();
        let mut env_fwd = EnvForwarding::default();
        let ws = Path::new("/tmp/nowhere-should-not-exist-cella-xyz");
        let fp_before = compute_mount_input_fingerprint(&settings, &env_fwd, ws);
        env_fwd.mounts.push(cella_env::ForwardMount {
            source: "/ssh-sock".to_string(),
            target: "/ssh-sock".to_string(),
        });
        let fp_after = compute_mount_input_fingerprint(&settings, &env_fwd, ws);
        assert_ne!(
            fp_before, fp_after,
            "fingerprint must change when an env_fwd mount is added"
        );
    }

    // -----------------------------------------------------------------------
    // validate_base_compose_against_reserved_agent (Finding 1, round 7)
    // -----------------------------------------------------------------------

    /// Build a `ResolvedComposeConfig` with per-service volumes AND top-level
    /// volume declarations.
    fn make_resolved_with_volumes(
        service: &str,
        svc_volumes: Vec<serde_json::Value>,
        top_level_volumes: HashMap<String, serde_json::Value>,
    ) -> ResolvedComposeConfig {
        let mut services = HashMap::new();
        services.insert(
            service.to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: svc_volumes,
                volumes_from: vec![],
                depends_on: serde_json::Value::default(),
            },
        );
        ResolvedComposeConfig {
            services,
            volumes: top_level_volumes,
        }
    }

    #[test]
    fn base_compose_rejected_for_agent_volume_alias_by_source() {
        let resolved = make_resolved_with_volumes(
            "app",
            vec![json!({"type": "volume", "source": "cella-agent", "target": "/tmp/agent-rw"})],
            HashMap::new(),
        );
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_err(),
            "expected rejection for source=cella-agent alias; got: {result:?}"
        );
    }

    #[test]
    fn base_compose_rejected_for_agent_target_path() {
        let resolved = make_resolved_with_volumes(
            "app",
            vec![json!({"type": "bind", "source": "/host", "target": "/cella/foo"})],
            HashMap::new(),
        );
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_err(),
            "expected rejection for target inside /cella; got: {result:?}"
        );
    }

    #[test]
    fn base_compose_rejected_when_top_level_alias_name_matches_agent() {
        let mut top_vols = HashMap::new();
        top_vols.insert("pretty-name".to_string(), json!({"name": "cella-agent"}));
        let resolved = make_resolved_with_volumes(
            "app",
            vec![json!({"type": "volume", "source": "pretty-name", "target": "/data"})],
            top_vols,
        );
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_err(),
            "expected rejection for top-level volume aliasing cella-agent; got: {result:?}"
        );
    }

    #[test]
    fn base_compose_ok_when_no_aliasing() {
        let resolved = make_resolved_with_volumes(
            "app",
            vec![json!({"type": "bind", "source": "/host", "target": "/app"})],
            HashMap::new(),
        );
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_ok(),
            "normal compose should pass; got: {result:?}"
        );
    }

    #[test]
    fn validator_rejects_sibling_service_aliasing_agent() {
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![], // clean primary
                volumes_from: vec![],
                depends_on: serde_json::Value::default(),
            },
        );
        services.insert(
            "sidecar".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![
                    json!({"type": "volume", "source": "cella-agent", "target": "/attack"}),
                ],
                volumes_from: vec![],
                depends_on: serde_json::Value::default(),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            volumes: HashMap::new(),
        };
        // All services are always checked — sidecar must cause failure.
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_err(),
            "sibling service aliasing agent must be rejected"
        );
    }

    #[test]
    fn validator_inspects_all_services_when_run_services_is_none() {
        // When run_services is None, all services are in the closure — a
        // sibling service aliasing the agent volume must be rejected.
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![],
                depends_on: serde_json::Value::default(),
            },
        );
        services.insert(
            "sibling".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![
                    json!({"type": "volume", "source": "cella-agent", "target": "/ignored"}),
                ],
                volumes_from: vec![],
                depends_on: serde_json::Value::default(),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            volumes: HashMap::new(),
        };
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_err(),
            "sibling service aliasing agent must be rejected"
        );
    }

    #[test]
    fn validator_rejects_service_using_volumes_from() {
        // volumes_from on the primary service inherits ALL volumes at runtime,
        // including cella's injected agent mount, without cella's read-only
        // protection. A sidecar using volumes_from: [primary] must be rejected.
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![],
                depends_on: serde_json::Value::default(),
            },
        );
        services.insert(
            "sidecar".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![json!("app")],
                depends_on: serde_json::Value::default(),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            volumes: HashMap::new(),
        };
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(result.is_err(), "volumes_from must be rejected");
    }

    #[test]
    fn volumes_from_unrelated_service_is_allowed() {
        // A migrator service using volumes_from on a db service is harmless —
        // db does not receive the injected agent volume, so no bypass occurs.
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![],
                depends_on: serde_json::Value::default(),
            },
        );
        services.insert(
            "db".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![],
                depends_on: serde_json::Value::default(),
            },
        );
        services.insert(
            "migrator".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![json!("db")],
                depends_on: serde_json::Value::default(),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            volumes: HashMap::new(),
        };
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_ok(),
            "volumes_from on unrelated service must be allowed; got: {result:?}"
        );
    }

    #[test]
    fn non_primary_service_may_target_cella_path() {
        // A sidecar service mounting /cella/something in its own container
        // filesystem does not shadow the primary's agent path — different
        // containers, different namespaces. This must be allowed.
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![],
                depends_on: serde_json::Value::default(),
            },
        );
        services.insert(
            "sidecar".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![
                    json!({"type": "bind", "source": "/host/data", "target": "/cella/something"}),
                ],
                volumes_from: vec![],
                depends_on: serde_json::Value::default(),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            volumes: HashMap::new(),
        };
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_ok(),
            "sidecar mounting /cella/* in its own namespace must be allowed; got: {result:?}"
        );
    }

    #[test]
    fn validator_rejects_short_form_aliasing_agent_volume() {
        // Short-form "cella-agent:/tmp/agent-rw" parses to source="cella-agent",
        // target="/tmp/agent-rw". Source has no leading '/' or '.' → inferred
        // type=volume → caught by Check 1 (source alias). Must still be rejected,
        // but via alias check rather than a fail-closed hard error.
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![json!("cella-agent:/tmp/agent-rw")],
                volumes_from: vec![],
                depends_on: serde_json::Value::default(),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            volumes: HashMap::new(),
        };
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_err(),
            "short-form volume-type alias must still be rejected via alias check"
        );
    }

    #[test]
    fn validator_allows_short_form_bind_with_source_matching_agent_name() {
        // Short-form "./cella-agent:/data" → source="./cella-agent", target="/data".
        // Source starts with '.' → inferred type=bind → not a volume alias.
        // A host directory literally named "cella-agent" is not a Docker volume.
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![json!("./cella-agent:/data")],
                volumes_from: vec![],
                depends_on: serde_json::Value::default(),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            volumes: HashMap::new(),
        };
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_ok(),
            "bind mount with literal dir path is not a volume alias; got: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Round-11 findings: compute_service_closure + refined validator scope
    // -----------------------------------------------------------------------

    #[test]
    fn closure_includes_transitive_dependencies() {
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![],
                depends_on: json!(["db"]),
            },
        );
        services.insert(
            "db".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![],
                depends_on: json!(["cache"]),
            },
        );
        services.insert(
            "cache".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![],
                depends_on: json!([]),
            },
        );
        services.insert(
            "unrelated".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![],
                depends_on: json!([]),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            volumes: HashMap::new(),
        };
        let app = "app".to_string();
        let closure = compute_service_closure(&resolved, Some(std::slice::from_ref(&app)));
        assert!(closure.contains("app"));
        assert!(closure.contains("db"));
        assert!(closure.contains("cache"));
        assert!(
            !closure.contains("unrelated"),
            "unrelated service not started"
        );
    }

    #[test]
    fn validator_skips_unrelated_services_when_run_services_set() {
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![],
                depends_on: json!([]),
            },
        );
        services.insert(
            "unrelated".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![json!({"type": "volume", "source": "cella-agent", "target": "/x"})],
                volumes_from: vec![],
                depends_on: json!([]),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            volumes: HashMap::new(),
        };
        let app = "app".to_string();
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            Some(std::slice::from_ref(&app)),
        );
        assert!(
            result.is_ok(),
            "unrelated sidecar (not in runServices closure) must be ignored; got: {result:?}"
        );
    }

    #[test]
    fn volumes_from_primary_readonly_suffix_is_allowed() {
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![],
                depends_on: json!([]),
            },
        );
        services.insert(
            "reader".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![json!("app:ro")],
                depends_on: json!([]),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            volumes: HashMap::new(),
        };
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_ok(),
            "read-only inheritance from primary is safe; got: {result:?}"
        );
    }

    #[test]
    fn volumes_from_primary_writable_is_rejected() {
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![],
                depends_on: json!([]),
            },
        );
        services.insert(
            "writer".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![json!("app")], // no mode = rw
                depends_on: json!([]),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            volumes: HashMap::new(),
        };
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_err(),
            "writable volumes_from on primary must be rejected"
        );
    }

    #[test]
    fn volumes_from_primary_object_read_only_true_is_allowed() {
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![],
                depends_on: json!([]),
            },
        );
        services.insert(
            "reader".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![json!({"source": "app", "read_only": true})],
                depends_on: json!([]),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            volumes: HashMap::new(),
        };
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_ok(),
            "object-form read_only: true inheritance is safe; got: {result:?}"
        );
    }

    #[test]
    fn bind_mount_with_source_matching_agent_name_is_allowed() {
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![json!({"type": "bind", "source": "cella-agent", "target": "/data"})],
                volumes_from: vec![],
                depends_on: json!([]),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            volumes: HashMap::new(),
        };
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_ok(),
            "bind mount with literal directory named 'cella-agent' is not a volume alias; got: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // validate_base_compose_against_reserved_agent — agent key attribute checks
    // (Finding 1, round 12)
    // -----------------------------------------------------------------------

    #[test]
    fn base_compose_rejected_when_agent_key_has_conflicting_fields() {
        // User base declares `volumes: cella-agent: { name: attacker-vol, driver: local }`.
        // Deep merge would carry those attributes into the final merged declaration,
        // breaking volume identity or causing an external+name conflict.
        let mut top_vols = HashMap::new();
        top_vols.insert(
            "cella-agent".to_string(),
            json!({"name": "attacker-vol", "driver": "local"}),
        );
        let resolved = make_resolved_with_volumes("app", vec![], top_vols);
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_err(),
            "user base compose cannot redefine agent volume with conflicting attributes"
        );
    }

    #[test]
    fn base_compose_ok_when_agent_key_is_bare() {
        // Bare `{}` carries no conflicting attributes — cella's override fully
        // pins `external: true` + `name: cella-agent`, so the merged declaration
        // is correct.  No rejection.
        let mut top_vols = HashMap::new();
        top_vols.insert("cella-agent".to_string(), json!({}));
        let resolved = make_resolved_with_volumes("app", vec![], top_vols);
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(result.is_ok(), "bare agent-key declaration is compatible");
    }

    #[test]
    fn base_compose_ok_when_agent_key_only_pins_same_name() {
        // `{ name: cella-agent }` is redundant but not conflicting — the merged
        // result has the same identity cella would produce.
        let mut top_vols = HashMap::new();
        top_vols.insert("cella-agent".to_string(), json!({"name": "cella-agent"}));
        let resolved = make_resolved_with_volumes("app", vec![], top_vols);
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_ok(),
            "matching name-only declaration is compatible"
        );
    }

    #[test]
    fn base_compose_rejected_when_agent_key_pins_different_name() {
        // `{ name: other }` would survive the merge and point the volume at a
        // different Docker volume identity — reject unconditionally.
        let mut top_vols = HashMap::new();
        top_vols.insert("cella-agent".to_string(), json!({"name": "other"}));
        let resolved = make_resolved_with_volumes("app", vec![], top_vols);
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_err(),
            "mismatched name on agent key must be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // hash_tool_host_paths / compute_mount_input_fingerprint host-path detection
    // (Finding 2, round 7)
    // -----------------------------------------------------------------------

    #[test]
    fn mount_input_fingerprint_changes_when_tool_host_path_changes() {
        // We cannot easily install/remove real tool dirs during a unit test, but
        // we CAN verify that changing a settings field that feeds into
        // host_nvim_config_dir (the config_path override) changes the fingerprint.
        // This exercises the hash_tool_host_paths path that invokes
        // host_nvim_config_dir with a non-None argument.
        let env_fwd = EnvForwarding::default();
        let ws = Path::new("/tmp/nowhere-should-not-exist-cella-xyz");

        let mut settings_a = cella_config::settings::Settings::default();
        settings_a.tools.nvim.forward_config = true;
        // config_path = None → host_nvim_config_dir checks default ~/.config/nvim
        let fp_a = compute_mount_input_fingerprint(&settings_a, &env_fwd, ws);

        let mut settings_b = cella_config::settings::Settings::default();
        settings_b.tools.nvim.forward_config = true;
        // config_path = Some fake path → different input to host_nvim_config_dir
        settings_b.tools.nvim.config_path = Some("/tmp/fake-nvim-config".to_string());
        let fp_b = compute_mount_input_fingerprint(&settings_b, &env_fwd, ws);

        assert_ne!(
            fp_a, fp_b,
            "fingerprint must change when nvim.config_path override differs"
        );
    }

    #[test]
    fn mount_input_fingerprint_canonicalizes_parent_git_path() {
        // The fingerprint must use the same canonicalize+fallback pattern as
        // `build_compose_mount_specs` so that linked worktrees with symlinked
        // .gitdir pointers produce consistent, comparable fingerprints.
        //
        // With a non-existent workspace root, `parent_git_dir` returns None and
        // `canonicalize()` is never called, but the fingerprint must still be
        // deterministic across calls (the fallback path is identical).
        let settings = cella_config::settings::Settings::default();
        let env_fwd = EnvForwarding::default();
        let ws = Path::new("/tmp/nowhere-cella-canonicalize-test-xyz");
        let fp1 = compute_mount_input_fingerprint(&settings, &env_fwd, ws);
        let fp2 = compute_mount_input_fingerprint(&settings, &env_fwd, ws);
        assert_eq!(fp1, fp2, "fingerprint must be deterministic across calls");
    }

    // -----------------------------------------------------------------------
    // validate_extra_named_volumes_against_base (Finding 3, round 9)
    // -----------------------------------------------------------------------

    fn make_volume_spec(source: &str) -> MountSpec {
        MountSpec {
            kind: MountKind::Volume,
            source: source.to_string(),
            target: "/mnt".to_string(),
            read_only: false,
            consistency: None,
        }
    }

    #[test]
    fn extra_named_volume_ok_when_no_base_entry() {
        // No top-level volume in base → no collision.
        let resolved = make_resolved_with_volumes("app", vec![], HashMap::new());
        let extras = vec![make_volume_spec("mycache")];
        assert!(validate_extra_named_volumes_against_base(&resolved, &extras).is_ok());
    }

    #[test]
    fn extra_named_volume_ok_when_base_pins_same_name() {
        // Base has explicit name: matching source → compatible.
        let mut top_vols = HashMap::new();
        top_vols.insert("mycache".to_string(), json!({"name": "mycache"}));
        let resolved = make_resolved_with_volumes("app", vec![], top_vols);
        let extras = vec![make_volume_spec("mycache")];
        assert!(
            validate_extra_named_volumes_against_base(&resolved, &extras).is_ok(),
            "explicit name matching source must be accepted"
        );
    }

    #[test]
    fn extra_named_volume_ok_when_base_is_external_with_matching_key() {
        // external: true survives the Compose deep-merge and would require the
        // volume to pre-exist on first run.  Must reject even though the key
        // name happens to match.
        let mut top_vols = HashMap::new();
        top_vols.insert("mycache".to_string(), json!({"external": true}));
        let resolved = make_resolved_with_volumes("app", vec![], top_vols);
        let extras = vec![make_volume_spec("mycache")];
        assert!(
            validate_extra_named_volumes_against_base(&resolved, &extras).is_err(),
            "external: true attribute is incompatible — must reject"
        );
    }

    #[test]
    fn extra_named_volume_rejected_when_base_is_external_with_different_name() {
        // external: true with a name: that differs from source, AND a service
        // references the key → mismatch causes identity divergence.
        let mut top_vols = HashMap::new();
        top_vols.insert(
            "mycache".to_string(),
            json!({"external": true, "name": "other"}),
        );
        let svc_vols = vec![json!({"type": "volume", "source": "mycache", "target": "/data"})];
        let resolved = make_resolved_with_volumes("app", svc_vols, top_vols);
        let extras = vec![make_volume_spec("mycache")];
        assert!(
            validate_extra_named_volumes_against_base(&resolved, &extras).is_err(),
            "external with mismatched name must reject when service references the key"
        );
    }

    #[test]
    fn extra_named_volume_ok_when_base_has_bare_key() {
        // Bare key carries no conflicting attributes — cella's `name: <source>`
        // pin fully defines identity after the deep merge.  Compatible.
        let mut top_vols = HashMap::new();
        top_vols.insert("mycache".to_string(), json!({}));
        let resolved = make_resolved_with_volumes("app", vec![], top_vols);
        let extras = vec![make_volume_spec("mycache")];
        assert!(
            validate_extra_named_volumes_against_base(&resolved, &extras).is_ok(),
            "bare key has no conflicting attributes — must be accepted"
        );
    }

    #[test]
    fn extra_named_volume_rejected_when_base_has_different_name() {
        // Explicit name mismatch AND a service references the key → reject.
        let mut top_vols = HashMap::new();
        top_vols.insert("mycache".to_string(), json!({"name": "app_db_vol"}));
        let svc_vols = vec![json!({"type": "volume", "source": "mycache", "target": "/data"})];
        let resolved = make_resolved_with_volumes("app", svc_vols, top_vols);
        let extras = vec![make_volume_spec("mycache")];
        assert!(
            validate_extra_named_volumes_against_base(&resolved, &extras).is_err(),
            "explicit name mismatch referenced by a service must reject"
        );
    }

    #[test]
    fn extra_non_volume_mounts_are_skipped() {
        // Bind and tmpfs mounts are not named volumes — always pass through.
        let mut top_vols = HashMap::new();
        top_vols.insert("mycache".to_string(), json!({}));
        let resolved = make_resolved_with_volumes("app", vec![], top_vols);
        let bind = MountSpec {
            kind: MountKind::Bind,
            source: "mycache".to_string(),
            target: "/mnt".to_string(),
            read_only: false,
            consistency: None,
        };
        assert!(
            validate_extra_named_volumes_against_base(&resolved, &[bind]).is_ok(),
            "bind mounts must be skipped by the volume-collision check"
        );
    }

    #[test]
    fn extra_named_volume_not_rejected_when_dedup_would_drop_it() {
        // The collision validator is called post-dedup, so when dedup removes
        // a user mount (because the base service already owns that target) the
        // validator never sees it.  This test exercises the validator directly
        // on an empty post-dedup list — the bare key in top-level volumes must
        // not trigger a rejection because there are no emittable specs to check.
        let mut top_vols = HashMap::new();
        top_vols.insert("mycache".to_string(), json!({})); // bare key, would normally conflict
        let resolved = make_resolved_with_volumes("app", vec![], top_vols);
        let extras: Vec<MountSpec> = vec![]; // post-dedup: empty
        assert!(
            validate_extra_named_volumes_against_base(&resolved, &extras).is_ok(),
            "empty post-dedup list should not trigger collision validation"
        );
    }

    #[test]
    fn extra_named_volume_ok_when_base_top_level_key_is_unreferenced() {
        // Base has a bare `mycache` key with no service reference.  Even if a
        // base service were to reference it, the bare key has no conflicting
        // attributes — cella's pin wins after the deep merge.  Compatible.
        let mut top_vols = HashMap::new();
        top_vols.insert("mycache".to_string(), json!({})); // bare key, no service reference
        let resolved = make_resolved_with_volumes("app", vec![], top_vols);
        let extras = vec![MountSpec {
            kind: MountKind::Volume,
            source: "mycache".to_string(),
            target: "/cache".to_string(),
            read_only: false,
            consistency: None,
        }];
        assert!(
            validate_extra_named_volumes_against_base(&resolved, &extras).is_ok(),
            "bare unreferenced key is compatible — must not block"
        );
    }

    #[test]
    fn extra_named_volume_rejected_when_bare_key_is_referenced_by_base_service() {
        // Base has a bare `mycache` key AND a service references it. A bare key
        // has no `name:` field, so Compose defaults to `<project>_mycache`.
        // Cella's literal-name pin would retarget that service's volume from
        // `<project>_mycache` to the global `mycache` — a data fork. Must reject.
        // The user must add `volumes.mycache.name: mycache` to opt in.
        let mut top_vols = HashMap::new();
        top_vols.insert("mycache".to_string(), json!({}));
        let svc_vols = vec![json!({"type": "volume", "source": "mycache", "target": "/data"})];
        let resolved = make_resolved_with_volumes("app", svc_vols, top_vols);
        let extras = vec![MountSpec {
            kind: MountKind::Volume,
            source: "mycache".to_string(),
            target: "/cache".to_string(),
            read_only: false,
            consistency: None,
        }];
        assert!(
            validate_extra_named_volumes_against_base(&resolved, &extras).is_err(),
            "bare key with service reference must be rejected — retargets project-scoped volume"
        );
    }

    #[test]
    fn extra_named_volume_rejected_when_base_has_driver_field() {
        // `driver` survives the Compose deep-merge and would change the backing
        // storage from the default.  Must reject regardless of service reference.
        let mut top_vols = HashMap::new();
        top_vols.insert("mycache".to_string(), json!({"driver": "nfs"}));
        let resolved = make_resolved_with_volumes("app", vec![], top_vols);
        let extras = vec![make_volume_spec("mycache")];
        assert!(
            validate_extra_named_volumes_against_base(&resolved, &extras).is_err(),
            "driver attribute is incompatible — must reject"
        );
    }

    #[test]
    fn extra_named_volume_rejected_when_base_has_external_field() {
        // `external: true` survives the Compose deep-merge and would require the
        // volume to pre-exist on first run, breaking fresh installs.  Must reject
        // regardless of whether any base service references the key.
        let mut top_vols = HashMap::new();
        top_vols.insert("mycache".to_string(), json!({"external": true}));
        let resolved = make_resolved_with_volumes("app", vec![], top_vols);
        let extras = vec![make_volume_spec("mycache")];
        assert!(
            validate_extra_named_volumes_against_base(&resolved, &extras).is_err(),
            "external base attribute survives merge — must reject"
        );
    }

    #[test]
    fn extra_named_volume_ok_when_base_has_only_matching_name() {
        // `{ name: mycache }` is redundant but not conflicting — after the merge
        // cella's pin still produces the same literal name.
        let mut top_vols = HashMap::new();
        top_vols.insert("mycache".to_string(), json!({"name": "mycache"}));
        let resolved = make_resolved_with_volumes("app", vec![], top_vols);
        let extras = vec![make_volume_spec("mycache")];
        assert!(
            validate_extra_named_volumes_against_base(&resolved, &extras).is_ok(),
            "matching name-only base declaration is compatible"
        );
    }

    #[test]
    fn extra_named_volume_ok_when_base_has_bare_empty_object() {
        // Bare `{}` carries no attributes that would survive the merge
        // and conflict with cella's pin.
        let mut top_vols = HashMap::new();
        top_vols.insert("mycache".to_string(), json!({}));
        let resolved = make_resolved_with_volumes("app", vec![], top_vols);
        let extras = vec![make_volume_spec("mycache")];
        assert!(
            validate_extra_named_volumes_against_base(&resolved, &extras).is_ok(),
            "bare empty-object declaration is compatible"
        );
    }

    // -----------------------------------------------------------------------
    // Round-13 findings: retarget of base-service project-scoped volumes
    // -----------------------------------------------------------------------

    #[test]
    fn extra_named_volume_rejected_when_base_service_uses_project_scoped_volume() {
        // Base service uses `mycache` as a volume (no top-level declaration →
        // project-scoped as `<project>_mycache`). Cella's literal-name pin would
        // retarget it from `<project>_mycache` to global `mycache` — reject.
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![json!({"type": "volume", "source": "mycache", "target": "/data"})],
                volumes_from: vec![],
                depends_on: json!([]),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            volumes: HashMap::new(),
        };
        let extras = vec![MountSpec {
            kind: MountKind::Volume,
            source: "mycache".to_string(),
            target: "/cache".to_string(),
            read_only: false,
            consistency: None,
        }];
        let result = validate_extra_named_volumes_against_base(&resolved, &extras);
        assert!(
            result.is_err(),
            "retargeting base service's project-scoped volume must reject"
        );
    }

    #[test]
    fn extra_named_volume_ok_when_base_service_uses_pinned_volume() {
        // Base service uses `mycache` AND top-level pins `name: mycache`.
        // Base service already resolves to the literal — cella's emission is
        // idempotent and safe.
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![json!({"type": "volume", "source": "mycache", "target": "/data"})],
                volumes_from: vec![],
                depends_on: json!([]),
            },
        );
        let mut top_vols = HashMap::new();
        top_vols.insert("mycache".to_string(), json!({"name": "mycache"}));
        let resolved = ResolvedComposeConfig {
            services,
            volumes: top_vols,
        };
        let extras = vec![MountSpec {
            kind: MountKind::Volume,
            source: "mycache".to_string(),
            target: "/cache".to_string(),
            read_only: false,
            consistency: None,
        }];
        let result = validate_extra_named_volumes_against_base(&resolved, &extras);
        assert!(
            result.is_ok(),
            "compatible pin exists — safe to emit; got: {result:?}"
        );
    }

    #[test]
    fn extra_named_volume_ok_when_base_service_uses_bind_mount_with_same_source_name() {
        // Base service uses `./mycache` as a BIND source (host directory named
        // `mycache`). Not a volume — cella's named-volume pin doesn't affect it.
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![json!({"type": "bind", "source": "./mycache", "target": "/data"})],
                volumes_from: vec![],
                depends_on: json!([]),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            volumes: HashMap::new(),
        };
        let extras = vec![MountSpec {
            kind: MountKind::Volume,
            source: "mycache".to_string(),
            target: "/cache".to_string(),
            read_only: false,
            consistency: None,
        }];
        let result = validate_extra_named_volumes_against_base(&resolved, &extras);
        assert!(
            result.is_ok(),
            "bind mount with similar source name is not a conflict; got: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Round-13 findings: container: form in volumes_from
    // -----------------------------------------------------------------------

    #[test]
    fn volumes_from_writable_container_form_is_rejected() {
        // `container:<name>` in writable mode inherits from an arbitrary running
        // container and could expose the managed agent volume. Must reject.
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![],
                depends_on: json!([]),
            },
        );
        services.insert(
            "sidecar".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![json!("container:other-project-primary")],
                depends_on: json!([]),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            volumes: HashMap::new(),
        };
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(result.is_err(), "writable container: form must be rejected");
    }

    #[test]
    fn volumes_from_readonly_container_form_is_allowed() {
        // `container:<name>:ro` is read-only — protection is preserved.
        let mut services = HashMap::new();
        services.insert(
            "app".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![],
                depends_on: json!([]),
            },
        );
        services.insert(
            "reader".to_string(),
            ResolvedService {
                image: None,
                build: None,
                volumes: vec![],
                volumes_from: vec![json!("container:external-container:ro")],
                depends_on: json!([]),
            },
        );
        let resolved = ResolvedComposeConfig {
            services,
            volumes: HashMap::new(),
        };
        let result = validate_base_compose_against_reserved_agent(
            &resolved,
            "cella-agent",
            "/cella",
            "app",
            None,
        );
        assert!(
            result.is_ok(),
            "read-only container: form preserves protection; got: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // resolve_bind_sources
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_bind_sources_makes_relative_paths_absolute() {
        let mut specs = vec![
            MountSpec::bind("./cache", "/cache"),
            MountSpec::bind("/already/absolute", "/abs"),
            MountSpec::bind("../sibling", "/sib"),
        ];
        // Non-existent path: canonicalize fails, raw join is returned as-is.
        let ws = Path::new("/tmp/fake-workspace");
        resolve_bind_sources(&mut specs, ws);
        // Relative `./cache` → joined with workspace (non-existent → raw join)
        assert_eq!(specs[0].source, "/tmp/fake-workspace/./cache");
        // Already-absolute source unchanged
        assert_eq!(specs[1].source, "/already/absolute");
        // `../sibling`: non-existent path → raw join returned
        assert_eq!(specs[2].source, "/tmp/fake-workspace/../sibling");
    }

    #[test]
    fn resolve_bind_sources_canonical_for_existing_path() {
        // Use /tmp which always exists — canonicalize must resolve it.
        let ws = Path::new("/tmp");
        let mut specs = vec![MountSpec::bind(".", "/container")];
        resolve_bind_sources(&mut specs, ws);
        // /tmp on Linux is often a symlink to /private/tmp (macOS) or resolves
        // to /tmp itself. Either way the result must be absolute.
        assert!(
            Path::new(&specs[0].source).is_absolute(),
            "resolved source must be absolute; got: {}",
            specs[0].source,
        );
    }

    #[test]
    fn resolve_bind_sources_ignores_non_bind_kinds() {
        let mut specs = vec![
            MountSpec::tmpfs("/mnt/shadow"),
            MountSpec {
                kind: MountKind::Volume,
                source: "mycache".to_string(),
                target: "/cache".to_string(),
                read_only: false,
                consistency: None,
            },
        ];
        let ws = Path::new("/tmp/workspace");
        resolve_bind_sources(&mut specs, ws);
        assert_eq!(specs[0].source, ""); // tmpfs source unchanged
        assert_eq!(specs[1].source, "mycache"); // volume source unchanged (it's a name, not path)
    }
}
