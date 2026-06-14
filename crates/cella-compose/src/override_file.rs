//! Generate the cella override Docker Compose YAML file.
//!
//! The override file is passed as the last `-f` flag to `docker compose`,
//! allowing cella to inject its customizations into the primary service:
//! image swap (for features), agent volume, environment variables, and labels.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

use cella_backend::{MountKind, MountSpec};
use cella_config::config_map::MergedSecurityConfig;

use crate::error::CellaComposeError;

/// A `BuildKit` secret to forward to compose builds.
#[derive(Debug, Clone)]
pub struct ComposeSecret {
    /// Secret identifier (matches `--mount=type=secret,id=<id>` in Dockerfile).
    pub id: String,
    /// Host file path containing the secret value.
    pub file: Option<PathBuf>,
    /// Environment variable containing the secret value.
    pub environment: Option<String>,
}

/// Configuration for generating the override compose file.
pub struct OverrideConfig {
    /// The primary service name (must match a service in the user's compose file).
    pub primary_service: String,
    /// Feature-enriched image tag (if features were built).
    pub image_override: Option<String>,
    /// Whether to override the service's CMD/ENTRYPOINT (defaults to false for compose).
    pub override_command: bool,
    /// Agent volume name (e.g., `cella-agent`).
    pub agent_volume_name: String,
    /// Agent volume mount target inside the container (e.g., `/cella`).
    pub agent_volume_target: String,
    /// Extra environment variables to inject (`KEY=VALUE` format).
    pub extra_env: Vec<String>,
    /// Extra labels for the primary service container.
    pub extra_labels: BTreeMap<String, String>,
    /// Override `build.dockerfile` for combined Dockerfile (compose + features).
    pub build_dockerfile: Option<PathBuf>,
    /// Override `build.target` for the features target stage.
    pub build_target: Option<String>,
    /// Override `build.context` for image-only services that need a synthetic build.
    pub build_context: Option<PathBuf>,
    /// Named build contexts for Docker `BuildKit` (e.g., feature content source).
    ///
    /// Emitted as `build.additional_contexts` in the compose override YAML.
    /// Requires Docker Compose >= 2.17.0.
    pub additional_contexts: BTreeMap<String, PathBuf>,
    /// `BuildKit` secrets forwarded to compose builds via the override YAML.
    pub build_secrets: Vec<ComposeSecret>,
    /// Image labels (`key=value`) to bake into the built service image.
    ///
    /// Emitted as `build.labels` (list form, `- "key=value"`) on the primary
    /// service, matching `docker compose`'s `build.labels` → image-label
    /// semantics. This is the compose equivalent of the single-container
    /// `docker build --label`, so the labels land on the built image, not on the
    /// runtime container (which is what `extra_labels` controls). Populated from
    /// `cella build --label`; empty preserves the existing YAML byte-for-byte.
    pub build_labels: Vec<String>,
    /// Extra volume mounts (long-form YAML) appended to the primary service's
    /// `volumes:` list after the agent volume.
    ///
    /// Populated by compose mount assembly in follow-up tasks (tool configs,
    /// SSH/GPG, parent-git, user mounts, feature mounts). An empty `Vec`
    /// preserves the existing YAML output byte-for-byte.
    pub extra_volumes: Vec<MountSpec>,
    /// Whether to grant the service a GPU. When `true`, emits a
    /// `deploy.resources.reservations.devices` block requesting capabilities
    /// `[gpu]` (matching the official compose GPU override). Decided by
    /// `hostRequirements.gpu` AND `--gpu-availability`.
    pub request_gpu: bool,
    /// Merged container security/runtime properties (containerUser, init,
    /// privileged, capAdd, securityOpt). Emitted as `user:`/`init:`/`privileged:`/
    /// `cap_add:`/`security_opt:` on the primary service, matching the official
    /// compose override. Shares the type with the single-container create path so
    /// both apply identical values.
    pub security: MergedSecurityConfig,
    /// Devcontainer feature `entrypoint` scripts, in install order, already
    /// `${devcontainerId}`-substituted. Each runs (verbatim, not `$`-escaped) in
    /// the wrapped entrypoint before the service's original entrypoint+command is
    /// `exec`d. Empty (with `override_command == false`) emits no entrypoint block
    /// at all, preserving the current override byte-for-byte.
    pub feature_entrypoints: Vec<String>,
    /// The resolved `userEntrypoint` for the wrapped entrypoint, per the official
    /// compose logic: `overrideCommand ? [] : (service.entrypoint || image
    /// entrypoint($-escaped))`. Emitted as JSON-quoted array elements after the
    /// `"-"` sentinel. Service-derived values arrive already compose-escaped (the
    /// resolved compose config re-escapes `$`→`$$`); image-derived values are
    /// `$`→`$$` escaped at resolution time.
    pub user_entrypoint: Vec<String>,
    /// The resolved `userCommand` for the wrapped entrypoint. `Some(cmd)` emits a
    /// `command:` key (when it differs from the compose-declared command);
    /// `None` omits it (the compose command already applies). Mirrors the
    /// official `userCommand !== composeCommand` gate.
    pub user_command: Option<Vec<String>>,
    /// Restrict the override to build-time only: skip the runtime sections that a
    /// `docker compose build` never needs — the agent volume mount and the
    /// top-level `volumes:` declarations. Set for the `--label`-only compose
    /// override, which adjusts the build but never runs the container, so it must
    /// not reference the (possibly-unprovisioned) external agent volume. The `up`
    /// path and the features build override leave this `false` to keep emitting
    /// the agent volume unchanged.
    pub build_only: bool,
}

/// The resolved `userEntrypoint`/`userCommand` for the wrapped compose entrypoint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserEntrypointCommand {
    /// Args appended after the `"-"` sentinel in the wrapped `entrypoint:` array.
    pub entrypoint: Vec<String>,
    /// The `command:` to emit (`None` = omit, value already equals the compose
    /// command; `Some` = emit, possibly empty).
    pub command: Option<Vec<String>>,
}

/// Escape `docker compose` interpolation in an image-derived value (`$` → `$$`).
///
/// Image `ENTRYPOINT`/`CMD` values are emitted into cella's `-f` override, which
/// `docker compose` interpolates. Doubling each `$` makes compose yield a literal
/// `$` for the shell (mirrors the official `c.replace(/\$/g, '$$$$')`). Service
/// values are NOT escaped here — the resolved compose config already returns them
/// in `$$`-escaped form.
fn escape_dollars(value: &str) -> String {
    value.replace('$', "$$")
}

/// Resolve the wrapped entrypoint's `userEntrypoint`/`userCommand`, mirroring the
/// official `dockerCompose.ts` logic exactly:
///
/// ```text
/// userEntrypoint = overrideCommand ? [] : (composeEntrypoint ?? imageEntrypoint($-esc))
/// userCommand    = overrideCommand ? [] : (composeCommand ?? (composeEntrypoint ? [] : imageCmd($-esc)))
/// emit command   = userCommand is NOT taken directly from composeCommand
/// ```
///
/// `compose_entrypoint`/`compose_command` are `Some` when the service declares the
/// key (even as an empty array — distinct from absent `None`), already in
/// compose-escaped form. Image values are `$`→`$$` escaped here. The returned
/// `command` is `None` exactly in the official "`userCommand === composeCommand`"
/// case (the service command applies unchanged, so no `command:` key is emitted).
#[must_use]
pub fn resolve_user_entrypoint_command(
    override_command: bool,
    compose_entrypoint: Option<&[String]>,
    compose_command: Option<&[String]>,
    image_entrypoint: &[String],
    image_cmd: &[String],
) -> UserEntrypointCommand {
    if override_command {
        // overrideCommand discards the original: userEntrypoint = userCommand = [].
        // userCommand ([]) is a fresh value, never === composeCommand, so it is
        // always emitted (as `command: []`).
        return UserEntrypointCommand {
            entrypoint: Vec::new(),
            command: Some(Vec::new()),
        };
    }

    // composeEntrypoint present (even empty) is used verbatim; absent falls back
    // to the image entrypoint ($-escaped) — official `composeEntrypoint || image`.
    let entrypoint = compose_entrypoint.map_or_else(
        || image_entrypoint.iter().map(|a| escape_dollars(a)).collect(),
        <[String]>::to_vec,
    );

    // userCommand and whether to emit it. The `command:` key is omitted only when
    // userCommand is taken directly from composeCommand (official `===`): i.e.
    // when composeCommand is present. When absent, userCommand is a fresh array
    // (always emitted) — `[]` if the service overrides the entrypoint (image CMD
    // ignored per the compose spec), otherwise the image CMD ($-escaped).
    let command = if compose_command.is_some() {
        None
    } else if compose_entrypoint.is_some() {
        Some(Vec::new())
    } else {
        Some(image_cmd.iter().map(|a| escape_dollars(a)).collect())
    };

    UserEntrypointCommand {
        entrypoint,
        command,
    }
}

/// Escape a string for embedding in a YAML double-quoted scalar (`key: "{v}"`).
///
/// The writer interpolates values into double-quoted scalars; without escaping,
/// a value containing `"` or `\` — notably the JSON `devcontainer.metadata`
/// label — terminates the scalar early and produces invalid YAML that breaks
/// `docker compose`. Escapes the double quote, backslash, and the C0 control
/// characters YAML requires be escaped in double-quoted scalars.
fn yaml_dq_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Emit one element of a YAML flow sequence as a double-quoted scalar.
///
/// Reuses [`yaml_dq_escape`] (escapes `"`/`\`/newlines/controls, leaves `$`
/// untouched) so an embedded literal `$$` — which `docker compose` interpolates
/// back to `$` when it reads this `-f` override — survives intact.
fn yaml_flow_scalar(value: &str) -> String {
    format!("\"{}\"", yaml_dq_escape(value))
}

/// Build the wrapped entrypoint shell script.
///
/// Mirrors the official compose override's `/bin/sh -c` script verbatim:
/// announce start, install a SIGTERM trap, run each feature entrypoint, `exec`
/// the service's original entrypoint+command (`"$@"`), then idle so the
/// container stays up. The `$$@`/`$$!` are emitted as literal double-dollars so
/// that, after `docker compose` interpolates this `-f` file (`$$`→`$`), the
/// shell receives `$@`/`$!`. Feature entrypoints are inserted verbatim (NOT
/// `$`-escaped), matching the official `customEntrypoints.join(...)`.
///
/// Deliberately contains NO agent restart loop: on the compose path the agent is
/// launched separately via `nohup` after `up` (see `compose_up::launch_agent_exec`).
fn build_wrapped_entrypoint_script(feature_entrypoints: &[String]) -> String {
    let mut script = String::from("echo Container started\ntrap \"exit 0\" 15\n");
    for ep in feature_entrypoints {
        script.push_str(ep);
        script.push('\n');
    }
    script.push_str("exec \"$$@\"\nwhile sleep 1 & wait $$!; do :; done");
    script
}

/// Emit the `entrypoint:` (and conditional `command:`) keys for the primary
/// service, wrapping feature entrypoints around the service's original
/// entrypoint+command.
///
/// Emits nothing when there are no feature entrypoints AND the command is not
/// being overridden — preserving the no-feature override byte-for-byte.
/// Otherwise writes:
/// - `entrypoint: ["/bin/sh", "-c", "<script>", "-"<, userEntrypoint args>]`
/// - `command: <userCommand>` — only when `user_command` is `Some` (the resolver
///   sets `None` when it equals the compose-declared command, matching the
///   official `userCommand !== composeCommand` gate).
fn write_entrypoint_section(yaml: &mut String, config: &OverrideConfig) {
    if config.feature_entrypoints.is_empty() && !config.override_command {
        return;
    }

    let script = build_wrapped_entrypoint_script(&config.feature_entrypoints);

    let mut elements = vec![
        yaml_flow_scalar("/bin/sh"),
        yaml_flow_scalar("-c"),
        yaml_flow_scalar(&script),
        yaml_flow_scalar("-"),
    ];
    for arg in &config.user_entrypoint {
        elements.push(yaml_flow_scalar(arg));
    }
    let _ = writeln!(yaml, "    entrypoint: [{}]", elements.join(", "));

    if let Some(ref command) = config.user_command {
        let items: Vec<String> = command.iter().map(|c| yaml_flow_scalar(c)).collect();
        let _ = writeln!(yaml, "    command: [{}]", items.join(", "));
    }
}

/// Generate the override compose YAML as a string.
///
/// The output is a valid Docker Compose file that overrides the primary service
/// with cella-specific configuration. It is designed to be passed as the last
/// `-f` flag so its values take precedence. String values are interpolated into
/// double-quoted scalars via [`yaml_dq_escape`] so embedded quotes/backslashes
/// (e.g. the JSON `devcontainer.metadata` label) stay valid YAML.
pub fn generate_override_yaml(config: &OverrideConfig) -> String {
    let mut yaml = String::from("# Auto-generated by cella. Do not edit.\n");

    // Services section
    yaml.push_str("services:\n");
    let _ = writeln!(yaml, "  {}:", config.primary_service);

    // Image override (for image-only services with features, to avoid retagging original)
    if let Some(ref image) = config.image_override {
        let _ = writeln!(yaml, "    image: \"{}\"", yaml_dq_escape(image));
    }

    // Build override (combined Dockerfile for compose + features, build secrets,
    // and/or image labels).
    write_build_section(&mut yaml, config);

    // Wrapped entrypoint that runs feature entrypoints, then execs the service's
    // original entrypoint+command. Emitted only when there is something to wrap
    // (feature entrypoints) or when the command is being overridden; otherwise no
    // entrypoint/command keys are written and the service runs unchanged.
    write_entrypoint_section(&mut yaml, config);

    // Container security/runtime properties (init, user, privileged, caps).
    write_security_section(&mut yaml, &config.security);

    // Environment variables
    if !config.extra_env.is_empty() {
        yaml.push_str("    environment:\n");
        for env in &config.extra_env {
            let _ = writeln!(yaml, "      - \"{}\"", yaml_dq_escape(env));
        }
    }

    // Labels
    if !config.extra_labels.is_empty() {
        yaml.push_str("    labels:\n");
        for (k, v) in &config.extra_labels {
            let _ = writeln!(yaml, "      {k}: \"{}\"", yaml_dq_escape(v));
        }
    }

    // GPU reservation (mirrors official's compose GPU override). Always
    // requests capabilities `[gpu]` with no count — parity with the single
    // container `--gpus all`.
    if config.request_gpu {
        yaml.push_str("    deploy:\n");
        yaml.push_str("      resources:\n");
        yaml.push_str("        reservations:\n");
        yaml.push_str("          devices:\n");
        yaml.push_str("            - capabilities: [gpu]\n");
    }

    // Runtime volume mounts — skipped for a build-only override (e.g. the
    // `--label`-only compose override), which adjusts the build but never runs
    // the container and so must not reference the agent volume.
    if !config.build_only {
        // Agent volume mount (read-only)
        yaml.push_str("    volumes:\n");
        let _ = writeln!(
            yaml,
            "      - {}:{}:ro",
            config.agent_volume_name, config.agent_volume_target
        );

        // Extra volume mounts (long-form YAML entries)
        for spec in &config.extra_volumes {
            yaml.push_str(&spec.to_compose_yaml_entry("      "));
        }
    }

    // Top-level secrets declarations (file/environment sources)
    if !config.build_secrets.is_empty() {
        yaml.push_str("secrets:\n");
        for secret in &config.build_secrets {
            let _ = writeln!(yaml, "  {}:", secret.id);
            if let Some(ref file) = secret.file {
                let _ = writeln!(
                    yaml,
                    "    file: \"{}\"",
                    yaml_dq_escape(&file.display().to_string())
                );
            }
            if let Some(ref env) = secret.environment {
                let _ = writeln!(yaml, "    environment: \"{}\"", yaml_dq_escape(env));
            }
        }
    }

    // Top-level volumes declarations.
    //
    // Always emit the agent volume first (external: true — cella pre-creates
    // it). Then, for each extra volume mount that uses a named volume
    // (MountKind::Volume with a non-empty source), emit a declaration with an
    // explicit `name:` pin so that Docker Compose uses the literal volume name
    // instead of project-scoping it as `<project>_<source>`. This preserves
    // parity with the single-container path which passes the literal source
    // name directly to the Docker API.
    //
    // docker compose -f merges top-level volume declarations (deep merge per
    // key across files). Emitting `name: <source>` unconditionally pins the
    // literal Docker volume name in the merged output regardless of what the
    // base compose file declares for that key (project-scoped, aliased via
    // `name:`, or `external: true` with no `name:`).
    if !config.build_only {
        yaml.push_str("volumes:\n");
        let _ = writeln!(yaml, "  {}:", config.agent_volume_name);
        yaml.push_str("    external: true\n");
        let mut emitted_volumes: BTreeSet<&str> = BTreeSet::new();
        for spec in &config.extra_volumes {
            if spec.kind == MountKind::Volume
                && !spec.source.is_empty()
                && emitted_volumes.insert(spec.source.as_str())
            {
                let _ = writeln!(yaml, "  {}:", spec.source);
                let _ = writeln!(yaml, "    name: {}", spec.source);
            }
        }
    }

    yaml
}

/// Emit the primary service's `build:` section (dockerfile/target/context +
/// additional contexts, build secrets, and image labels).
///
/// The section appears when any build override is present. `build_labels` can
/// stand alone (no dockerfile) when `--label` targets a service whose `build:` is
/// inherited from the base compose via `-f` merge — so it independently forces the
/// `build:` block. Image labels use list form (`- "key=value"`); values are user
/// free-form, so each is quoted + escaped (a value with `:`/`#`/space would
/// otherwise break the YAML or inject keys — same reason env/security are quoted).
fn write_build_section(yaml: &mut String, config: &OverrideConfig) {
    let has_build_section = config.build_dockerfile.is_some()
        || !config.build_secrets.is_empty()
        || !config.build_labels.is_empty();
    if !has_build_section {
        return;
    }
    yaml.push_str("    build:\n");
    if let Some(ref dockerfile) = config.build_dockerfile {
        let _ = writeln!(
            yaml,
            "      dockerfile: \"{}\"",
            yaml_dq_escape(&dockerfile.display().to_string())
        );
        if let Some(ref target) = config.build_target {
            let _ = writeln!(yaml, "      target: \"{}\"", yaml_dq_escape(target));
        }
        if let Some(ref context) = config.build_context {
            let _ = writeln!(
                yaml,
                "      context: \"{}\"",
                yaml_dq_escape(&context.display().to_string())
            );
        }
        if !config.additional_contexts.is_empty() {
            yaml.push_str("      additional_contexts:\n");
            for (name, path) in &config.additional_contexts {
                let _ = writeln!(yaml, "        - {name}={}", path.display());
            }
        }
    }
    if !config.build_secrets.is_empty() {
        yaml.push_str("      secrets:\n");
        for secret in &config.build_secrets {
            let _ = writeln!(yaml, "        - {}", secret.id);
        }
    }
    if !config.build_labels.is_empty() {
        yaml.push_str("      labels:\n");
        for label in &config.build_labels {
            let _ = writeln!(yaml, "        - \"{}\"", yaml_dq_escape(label));
        }
    }
}

/// Emit the container security/runtime section (init, user, privileged, capAdd,
/// securityOpt) onto the primary service, matching the official compose override
/// (`dockerCompose.ts` emits the same keys on the service definition).
fn write_security_section(yaml: &mut String, sec: &MergedSecurityConfig) {
    // Values come from devcontainer.json / image metadata, so quote and escape
    // them (consistent with the labels/env/image emission above). A double-quoted,
    // escaped scalar keeps a stray `:`/`#`/`"`/newline inside the value instead of
    // breaking the override or injecting new YAML keys.
    if sec.init {
        yaml.push_str("    init: true\n");
    }
    if let Some(ref user) = sec.container_user {
        let _ = writeln!(yaml, "    user: \"{}\"", yaml_dq_escape(user));
    }
    if sec.privileged {
        yaml.push_str("    privileged: true\n");
    }
    if !sec.cap_add.is_empty() {
        yaml.push_str("    cap_add:\n");
        for cap in &sec.cap_add {
            let _ = writeln!(yaml, "      - \"{}\"", yaml_dq_escape(cap));
        }
    }
    if !sec.security_opt.is_empty() {
        yaml.push_str("    security_opt:\n");
        for opt in &sec.security_opt {
            let _ = writeln!(yaml, "      - \"{}\"", yaml_dq_escape(opt));
        }
    }
}

/// Write the override file to disk, creating parent directories as needed.
///
/// # Errors
///
/// Returns an error if the parent directories cannot be created or if writing
/// the file fails.
pub fn write_override_file(path: &Path, content: &str) -> Result<(), CellaComposeError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

/// Remove the override file and its parent directory if empty.
pub fn cleanup_override_file(path: &Path) {
    let _ = std::fs::remove_file(path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir(parent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> OverrideConfig {
        OverrideConfig {
            primary_service: "app".to_string(),
            image_override: None,
            override_command: false,
            agent_volume_name: "cella-agent".to_string(),
            agent_volume_target: "/cella".to_string(),
            extra_env: Vec::new(),
            extra_labels: BTreeMap::new(),
            build_dockerfile: None,
            build_target: None,
            build_context: None,
            additional_contexts: BTreeMap::new(),
            build_secrets: Vec::new(),
            build_labels: Vec::new(),
            extra_volumes: Vec::new(),
            request_gpu: false,
            security: MergedSecurityConfig::default(),
            feature_entrypoints: Vec::new(),
            user_entrypoint: Vec::new(),
            user_command: None,
            build_only: false,
        }
    }

    #[test]
    fn runtime_security_properties_emitted() {
        let mut config = base_config();
        config.security.container_user = Some("vscode".to_string());
        config.security.init = true;
        config.security.privileged = true;
        config.security.cap_add = vec!["SYS_PTRACE".to_string(), "NET_ADMIN".to_string()];
        config.security.security_opt = vec!["seccomp=unconfined".to_string()];
        let yaml = generate_override_yaml(&config);
        assert!(yaml.contains("    init: true\n"), "yaml:\n{yaml}");
        assert!(yaml.contains("    user: \"vscode\"\n"), "yaml:\n{yaml}");
        assert!(yaml.contains("    privileged: true\n"), "yaml:\n{yaml}");
        assert!(yaml.contains("    cap_add:\n      - \"SYS_PTRACE\"\n      - \"NET_ADMIN\"\n"));
        assert!(yaml.contains("    security_opt:\n      - \"seccomp=unconfined\"\n"));
    }

    #[test]
    fn runtime_security_properties_omitted_when_default() {
        let yaml = generate_override_yaml(&base_config());
        assert!(!yaml.contains("init:"));
        assert!(!yaml.contains("user:"));
        assert!(!yaml.contains("privileged:"));
        assert!(!yaml.contains("cap_add:"));
        assert!(!yaml.contains("security_opt:"));
    }

    #[test]
    fn yaml_dq_escape_handles_quotes_backslashes_and_controls() {
        assert_eq!(yaml_dq_escape("plain/path.json"), "plain/path.json");
        assert_eq!(yaml_dq_escape(r#"a"b"#), r#"a\"b"#);
        assert_eq!(yaml_dq_escape(r"a\b"), r"a\\b");
        assert_eq!(yaml_dq_escape("a\nb"), r"a\nb");
        assert_eq!(yaml_dq_escape("a\tb"), r"a\tb");
    }

    #[test]
    fn metadata_label_json_value_round_trips_through_yaml() {
        // Regression: `devcontainer.metadata` is raw JSON (contains `"`). Without
        // escaping, `key: "{v}"` produced invalid YAML that broke `docker compose`.
        // The generated override must parse and the label must round-trip intact.
        let metadata =
            r#"[{"id":"ghcr.io/x/y:1"},{"remoteUser":"vscode","containerEnv":{"FOO":"a:b#c"}}]"#;
        let mut config = base_config();
        config
            .extra_labels
            .insert("devcontainer.metadata".to_string(), metadata.to_string());
        config
            .extra_labels
            .insert("devcontainer.local_folder".to_string(), "/ws".to_string());
        let yaml = generate_override_yaml(&config);

        let parsed: yaml_serde::Value =
            yaml_serde::from_str(&yaml).expect("generated override must be valid YAML");
        let labels = &parsed["services"]["app"]["labels"];
        assert_eq!(labels["devcontainer.metadata"].as_str(), Some(metadata));
        assert_eq!(labels["devcontainer.local_folder"].as_str(), Some("/ws"));
    }

    #[test]
    fn minimal_override() {
        let config = base_config();
        let yaml = generate_override_yaml(&config);
        insta::assert_snapshot!(yaml, @"
        # Auto-generated by cella. Do not edit.
        services:
          app:
            volumes:
              - cella-agent:/cella:ro
        volumes:
          cella-agent:
            external: true
        ");
    }

    #[test]
    fn with_image_override() {
        let mut config = base_config();
        config.image_override = Some("cella-img-myapp-abc12345-def67890".to_string());
        let yaml = generate_override_yaml(&config);
        assert!(yaml.contains("image: \"cella-img-myapp-abc12345-def67890\""));
    }

    #[test]
    fn build_only_omits_runtime_volume_sections() {
        // A build-only override (the `--label`-only compose path) adjusts the
        // build but never runs the container, so it must omit the agent volume —
        // both the service `volumes:` mount and the top-level `volumes:`
        // declaration, the latter of which would otherwise force `external: true`
        // validation of an unprovisioned volume on `docker compose build`.
        let mut config = base_config();
        config.build_only = true;
        config.build_labels = vec!["cella.test=1".to_string()];
        let yaml = generate_override_yaml(&config);

        assert!(
            !yaml.contains("volumes:"),
            "build_only override must omit every volumes section; yaml:\n{yaml}"
        );
        assert!(
            !yaml.contains("cella-agent"),
            "build_only override must not reference the agent volume; yaml:\n{yaml}"
        );

        // Still a valid compose file that carries the build label.
        let parsed: yaml_serde::Value =
            yaml_serde::from_str(&yaml).expect("build_only override must be valid YAML");
        assert_eq!(
            parsed["services"]["app"]["build"]["labels"][0].as_str(),
            Some("cella.test=1")
        );
    }

    #[test]
    fn build_only_false_keeps_volume_sections() {
        // The default (up / features-build path) still emits the agent volume.
        let config = base_config();
        assert!(!config.build_only);
        let yaml = generate_override_yaml(&config);
        assert!(yaml.contains("cella-agent:/cella:ro"), "yaml:\n{yaml}");
        assert!(yaml.contains("external: true"), "yaml:\n{yaml}");
    }

    #[test]
    fn build_section_dockerfile_and_context_are_escaped() {
        // Regression (Windows paths): a `build.dockerfile`/`build.context` with
        // backslashes (or a `"` in `target`) must stay inside the double-quoted
        // YAML scalar instead of terminating it early or being read as an escape
        // (`\t` → TAB), which would break `docker compose`.
        let mut config = base_config();
        config.build_dockerfile = Some(PathBuf::from(r"C:\tmp\My Dockerfile"));
        config.build_context = Some(PathBuf::from(r"C:\tmp\ctx"));
        config.build_target = Some("stage\"x".to_string());
        let yaml = generate_override_yaml(&config);

        let parsed: yaml_serde::Value =
            yaml_serde::from_str(&yaml).expect("override with backslash paths must be valid YAML");
        let build = &parsed["services"]["app"]["build"];
        assert_eq!(build["dockerfile"].as_str(), Some(r"C:\tmp\My Dockerfile"));
        assert_eq!(build["context"].as_str(), Some(r"C:\tmp\ctx"));
        assert_eq!(build["target"].as_str(), Some("stage\"x"));
    }

    #[test]
    fn gpu_request_emits_device_reservation() {
        let mut config = base_config();
        config.request_gpu = true;
        let yaml = generate_override_yaml(&config);
        assert!(yaml.contains("deploy:"));
        assert!(yaml.contains("reservations:"));
        assert!(yaml.contains("- capabilities: [gpu]"));
    }

    #[test]
    fn no_gpu_request_omits_device_reservation() {
        let config = base_config();
        let yaml = generate_override_yaml(&config);
        assert!(!yaml.contains("deploy:"));
        assert!(!yaml.contains("capabilities: [gpu]"));
    }

    #[test]
    fn with_command_override() {
        // overrideCommand=true discards the service's original entrypoint/command:
        // userEntrypoint = [] (no args after "-"), userCommand = Some([]) so the
        // wrapped entrypoint's `exec "$@"` runs nothing and the keepalive loop
        // holds the container open. (The resolver produces these values; here we
        // set them directly to exercise the emitter.)
        let mut config = base_config();
        config.override_command = true;
        config.user_entrypoint = Vec::new();
        config.user_command = Some(Vec::new());
        let yaml = generate_override_yaml(&config);
        assert!(
            yaml.contains(
                "    entrypoint: [\"/bin/sh\", \"-c\", \
                 \"echo Container started\\ntrap \\\"exit 0\\\" 15\\n\
                 exec \\\"$$@\\\"\\nwhile sleep 1 & wait $$!; do :; done\", \"-\"]\n"
            ),
            "yaml:\n{yaml}"
        );
        assert!(yaml.contains("    command: []\n"), "yaml:\n{yaml}");
    }

    #[test]
    fn without_command_override() {
        let config = base_config();
        let yaml = generate_override_yaml(&config);
        assert!(!yaml.contains("entrypoint:"));
        assert!(!yaml.contains("command:"));
    }

    #[test]
    fn with_env_vars() {
        let mut config = base_config();
        config.extra_env = vec![
            "CELLA_DAEMON_PORT=9876".to_string(),
            "CELLA_AUTH_TOKEN=abc123".to_string(),
        ];
        let yaml = generate_override_yaml(&config);
        assert!(yaml.contains("environment:"));
        assert!(yaml.contains("CELLA_DAEMON_PORT=9876"));
        assert!(yaml.contains("CELLA_AUTH_TOKEN=abc123"));
    }

    #[test]
    fn with_labels() {
        let mut config = base_config();
        config
            .extra_labels
            .insert("dev.cella.tool".to_string(), "cella".to_string());
        config.extra_labels.insert(
            "dev.cella.compose_project".to_string(),
            "myproject".to_string(),
        );
        let yaml = generate_override_yaml(&config);
        assert!(yaml.contains("labels:"));
        assert!(yaml.contains("dev.cella.tool: \"cella\""));
        assert!(yaml.contains("dev.cella.compose_project: \"myproject\""));
    }

    #[test]
    fn full_override() {
        let mut config = base_config();
        config.image_override = Some("cella-img-app-12345678".to_string());
        config.override_command = true;
        // overrideCommand=true resolves to an empty userCommand (the resolver
        // emits Some([])), which clears the original command; the wrapped
        // entrypoint's keepalive loop keeps the container up.
        config.user_command = Some(Vec::new());
        config.extra_env = vec!["CELLA_DAEMON_PORT=9876".to_string()];
        config
            .extra_labels
            .insert("dev.cella.tool".to_string(), "cella".to_string());
        let yaml = generate_override_yaml(&config);
        insta::assert_snapshot!(yaml, @r#"
        # Auto-generated by cella. Do not edit.
        services:
          app:
            image: "cella-img-app-12345678"
            entrypoint: ["/bin/sh", "-c", "echo Container started\ntrap \"exit 0\" 15\nexec \"$$@\"\nwhile sleep 1 & wait $$!; do :; done", "-"]
            command: []
            environment:
              - "CELLA_DAEMON_PORT=9876"
            labels:
              dev.cella.tool: "cella"
            volumes:
              - cella-agent:/cella:ro
        volumes:
          cella-agent:
            external: true
        "#);
    }

    #[test]
    fn full_override_with_extra_volumes() {
        let mut config = base_config();
        config.image_override = Some("cella-img-app-12345678".to_string());
        config.override_command = true;
        config.user_command = Some(Vec::new());
        config.extra_env = vec!["CELLA_DAEMON_PORT=9876".to_string()];
        config
            .extra_labels
            .insert("dev.cella.tool".to_string(), "cella".to_string());
        config.extra_volumes = vec![
            MountSpec::bind("/home/u/.claude", "/root/.claude"),
            MountSpec::tmpfs("/root/.claude/plugins"),
        ];
        let yaml = generate_override_yaml(&config);
        insta::assert_snapshot!(yaml, @r#"
        # Auto-generated by cella. Do not edit.
        services:
          app:
            image: "cella-img-app-12345678"
            entrypoint: ["/bin/sh", "-c", "echo Container started\ntrap \"exit 0\" 15\nexec \"$$@\"\nwhile sleep 1 & wait $$!; do :; done", "-"]
            command: []
            environment:
              - "CELLA_DAEMON_PORT=9876"
            labels:
              dev.cella.tool: "cella"
            volumes:
              - cella-agent:/cella:ro
              - type: bind
                source: "/home/u/.claude"
                target: "/root/.claude"
                bind:
                  create_host_path: false
              - type: tmpfs
                target: "/root/.claude/plugins"
        volumes:
          cella-agent:
            external: true
        "#);
    }

    #[test]
    fn with_build_override() {
        let mut config = base_config();
        config.build_dockerfile = Some(PathBuf::from(
            "/home/user/.cella/compose/proj/Dockerfile.combined",
        ));
        config.build_target = Some("dev_containers_target_stage".to_string());
        let yaml = generate_override_yaml(&config);
        assert!(yaml.contains("build:"));
        assert!(yaml.contains("dockerfile:"));
        assert!(yaml.contains("Dockerfile.combined"));
        assert!(yaml.contains("target: \"dev_containers_target_stage\""));
        // No context override for build-based services
        assert!(!yaml.contains("context:"));
    }

    #[test]
    fn with_build_override_and_context() {
        let mut config = base_config();
        config.build_dockerfile = Some(PathBuf::from("/tmp/Dockerfile.combined"));
        config.build_target = Some("dev_containers_target_stage".to_string());
        config.build_context = Some(PathBuf::from("/tmp/features-context"));
        config.image_override = Some("cella-img-app-features".to_string());
        let yaml = generate_override_yaml(&config);
        assert!(yaml.contains("image: \"cella-img-app-features\""));
        assert!(yaml.contains("build:"));
        assert!(yaml.contains("dockerfile:"));
        assert!(yaml.contains("target:"));
        assert!(yaml.contains("context: \"/tmp/features-context\""));
    }

    #[test]
    fn build_labels_emitted_under_build_section() {
        // `--label` on a features build: the labels join the existing `build:`
        // section (dockerfile + labels) as a quoted list.
        let mut config = base_config();
        config.build_dockerfile = Some(PathBuf::from("/tmp/Dockerfile.combined"));
        config.build_target = Some("dev_containers_target_stage".to_string());
        config.build_labels = vec!["cella.test=1".to_string(), "foo=bar".to_string()];
        let yaml = generate_override_yaml(&config);
        assert!(yaml.contains("    build:\n"), "yaml:\n{yaml}");
        assert!(yaml.contains("      labels:\n"), "yaml:\n{yaml}");
        assert!(
            yaml.contains("        - \"cella.test=1\"\n"),
            "yaml:\n{yaml}"
        );
        assert!(yaml.contains("        - \"foo=bar\"\n"), "yaml:\n{yaml}");
    }

    #[test]
    fn build_labels_only_emits_build_section_without_dockerfile() {
        // Sub-case 2: a labels-only override (no dockerfile/context — those are
        // inherited from the base compose via `-f` merge). The `build:` block
        // must still appear, carrying only `labels:`, and the YAML must be valid.
        let mut config = base_config();
        config.build_labels = vec!["cella.test=2".to_string()];
        let yaml = generate_override_yaml(&config);
        insta::assert_snapshot!(yaml, @r#"
        # Auto-generated by cella. Do not edit.
        services:
          app:
            build:
              labels:
                - "cella.test=2"
            volumes:
              - cella-agent:/cella:ro
        volumes:
          cella-agent:
            external: true
        "#);
        // No dockerfile/context/target keys on a labels-only build.
        assert!(!yaml.contains("dockerfile:"), "yaml:\n{yaml}");
        assert!(!yaml.contains("context:"), "yaml:\n{yaml}");
        assert!(!yaml.contains("target:"), "yaml:\n{yaml}");
    }

    #[test]
    fn build_labels_with_special_chars_round_trip_through_yaml() {
        // Label values are user free-form: a value with `:` (and `#`/space) must
        // survive as a single string, not break the YAML or inject a new key.
        // Mirrors `metadata_label_json_value_round_trips_through_yaml`.
        let mut config = base_config();
        config.build_labels = vec![
            "org.opencontainers.image.description=Foo: bar #baz".to_string(),
            "key.with.equals=a=b=c".to_string(),
        ];
        let yaml = generate_override_yaml(&config);
        let parsed: yaml_serde::Value =
            yaml_serde::from_str(&yaml).expect("generated override must be valid YAML");
        let labels = &parsed["services"]["app"]["build"]["labels"];
        let entries: Vec<&str> = labels
            .as_sequence()
            .expect("build.labels must be a sequence")
            .iter()
            .map(|v| v.as_str().expect("each label entry is a string"))
            .collect();
        assert_eq!(
            entries,
            vec![
                "org.opencontainers.image.description=Foo: bar #baz",
                "key.with.equals=a=b=c",
            ]
        );
    }

    #[test]
    fn no_build_labels_omits_labels_from_build_section() {
        // Without build_labels, a features-style build override must not emit a
        // `labels:` line under `build:` (only the existing dockerfile keys).
        let mut config = base_config();
        config.build_dockerfile = Some(PathBuf::from("/tmp/Dockerfile.combined"));
        let yaml = generate_override_yaml(&config);
        let build_section = yaml
            .split("    build:\n")
            .nth(1)
            .expect("build section present");
        assert!(
            !build_section.contains("labels:"),
            "build.labels must be absent when build_labels is empty; yaml:\n{yaml}"
        );
    }

    #[test]
    fn with_additional_contexts() {
        let mut config = base_config();
        config.build_dockerfile = Some(PathBuf::from("/tmp/Dockerfile.combined"));
        config.build_target = Some("dev_containers_target_stage".to_string());
        config.additional_contexts.insert(
            "dev_containers_feature_content_source".to_string(),
            PathBuf::from("/home/user/.cache/cella/features/builds/abc123"),
        );
        let yaml = generate_override_yaml(&config);
        assert!(yaml.contains("additional_contexts:"));
        assert!(yaml.contains(
            "dev_containers_feature_content_source=/home/user/.cache/cella/features/builds/abc123"
        ));
    }

    #[test]
    fn with_build_override_context_and_additional_contexts() {
        let mut config = base_config();
        config.build_dockerfile = Some(PathBuf::from("/tmp/Dockerfile.combined"));
        config.build_target = Some("dev_containers_target_stage".to_string());
        config.build_context = Some(PathBuf::from("/tmp/empty-context"));
        config.image_override = Some("cella-img-app-features".to_string());
        config.additional_contexts.insert(
            "dev_containers_feature_content_source".to_string(),
            PathBuf::from("/tmp/features-context"),
        );
        let yaml = generate_override_yaml(&config);
        insta::assert_snapshot!(yaml, @r#"
        # Auto-generated by cella. Do not edit.
        services:
          app:
            image: "cella-img-app-features"
            build:
              dockerfile: "/tmp/Dockerfile.combined"
              target: "dev_containers_target_stage"
              context: "/tmp/empty-context"
              additional_contexts:
                - dev_containers_feature_content_source=/tmp/features-context
            volumes:
              - cella-agent:/cella:ro
        volumes:
          cella-agent:
            external: true
        "#);
    }

    #[test]
    fn write_and_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project").join("docker-compose.cella.yml");

        write_override_file(&path, "test content").unwrap();
        assert!(path.exists());

        cleanup_override_file(&path);
        assert!(!path.exists());
    }

    #[test]
    fn additional_contexts_without_build_dockerfile_are_ignored() {
        let mut config = base_config();
        // Set additional_contexts but NOT build_dockerfile
        config
            .additional_contexts
            .insert("feature_src".to_string(), PathBuf::from("/tmp/features"));
        let yaml = generate_override_yaml(&config);
        // additional_contexts should NOT appear since build_dockerfile is None
        assert!(
            !yaml.contains("additional_contexts"),
            "additional_contexts should be omitted when build_dockerfile is None"
        );
    }

    #[test]
    fn multiple_additional_contexts_sorted() {
        let mut config = base_config();
        config.build_dockerfile = Some(PathBuf::from("/tmp/Dockerfile.combined"));
        config
            .additional_contexts
            .insert("zulu_context".to_string(), PathBuf::from("/tmp/zulu"));
        config
            .additional_contexts
            .insert("alpha_context".to_string(), PathBuf::from("/tmp/alpha"));
        config
            .additional_contexts
            .insert("mike_context".to_string(), PathBuf::from("/tmp/mike"));
        let yaml = generate_override_yaml(&config);
        // BTreeMap guarantees sorted order
        let alpha_pos = yaml.find("alpha_context").expect("alpha_context not found");
        let mike_pos = yaml.find("mike_context").expect("mike_context not found");
        let zulu_pos = yaml.find("zulu_context").expect("zulu_context not found");
        assert!(
            alpha_pos < mike_pos && mike_pos < zulu_pos,
            "additional_contexts should appear in BTreeMap (alphabetical) order"
        );
    }

    #[test]
    fn special_characters_in_paths_are_quoted() {
        let mut config = base_config();
        config.build_dockerfile = Some(PathBuf::from("/tmp/my project/Dockerfile"));
        config.build_target = Some("dev_stage".to_string());
        let yaml = generate_override_yaml(&config);
        // The path with spaces should appear in the YAML
        assert!(yaml.contains("my project"));
        // The dockerfile value should be quoted
        assert!(yaml.contains(r#"dockerfile: "/tmp/my project/Dockerfile""#));
    }

    #[test]
    fn with_single_bind_mount() {
        let mut config = base_config();
        config.extra_volumes = vec![MountSpec::bind("/home/u/.claude", "/root/.claude")];
        let yaml = generate_override_yaml(&config);
        insta::assert_snapshot!(yaml, @r#"
        # Auto-generated by cella. Do not edit.
        services:
          app:
            volumes:
              - cella-agent:/cella:ro
              - type: bind
                source: "/home/u/.claude"
                target: "/root/.claude"
                bind:
                  create_host_path: false
        volumes:
          cella-agent:
            external: true
        "#);
    }

    #[test]
    fn with_tmpfs_over_bind() {
        let mut config = base_config();
        config.extra_volumes = vec![
            MountSpec::bind("/home/u/.claude", "/root/.claude"),
            MountSpec::tmpfs("/root/.claude/plugins"),
        ];
        let yaml = generate_override_yaml(&config);
        assert!(yaml.contains("target: \"/root/.claude/plugins\""));
        assert!(yaml.contains("type: tmpfs"));
        let bind_pos = yaml.find("type: bind").unwrap();
        let tmpfs_pos = yaml.find("type: tmpfs").unwrap();
        assert!(bind_pos < tmpfs_pos, "bind must precede tmpfs in output");
    }

    #[test]
    fn with_readonly_bind() {
        let mut config = base_config();
        config.extra_volumes = vec![MountSpec::bind("/a", "/a").read_only()];
        let yaml = generate_override_yaml(&config);
        assert!(yaml.contains("read_only: true"));
    }

    #[test]
    fn empty_extra_volumes_does_not_alter_existing_yaml() {
        // Sanity: the pre-existing minimal_override snapshot must still apply.
        let config = base_config();
        let yaml = generate_override_yaml(&config);
        assert!(!yaml.contains("type: bind"));
        assert!(!yaml.contains("type: tmpfs"));
        // Agent volume short-form line is still present.
        assert!(yaml.contains("cella-agent:/cella:ro"));
    }

    // -----------------------------------------------------------------------
    // Finding 2: named volume top-level declarations
    // -----------------------------------------------------------------------

    fn named_volume_spec(source: &str, target: &str) -> MountSpec {
        MountSpec {
            kind: MountKind::Volume,
            source: source.to_string(),
            target: target.to_string(),
            read_only: false,
            consistency: None,
        }
    }

    #[test]
    fn named_volume_mount_emits_top_level_declaration() {
        // A user `type=volume,source=mycache,target=/cache` mount must produce
        // a top-level `mycache:` declaration with a `name: mycache` pin so
        // compose uses the literal volume name instead of project-scoping it
        // (e.g. `<project>_mycache`). It must NOT be declared as `external: true`
        // because that would break fresh installs.
        let mut config = base_config();
        config.extra_volumes = vec![named_volume_spec("mycache", "/cache")];
        let yaml = generate_override_yaml(&config);
        assert!(
            yaml.contains("  mycache:"),
            "top-level mycache declaration must be emitted; yaml:\n{yaml}"
        );
        assert!(
            yaml.contains("    name: mycache"),
            "literal name pin must be emitted; yaml:\n{yaml}"
        );
        // Only the agent volume should be external: true — not user-managed volumes.
        let external_count = yaml.matches("external: true").count();
        assert_eq!(
            external_count, 1,
            "only the agent volume should be declared external; yaml:\n{yaml}"
        );
        // The agent volume declaration must still be present.
        assert!(yaml.contains("cella-agent:"));
    }

    #[test]
    fn named_volume_not_emitted_as_external() {
        // Regression test: extra named volumes must use bare key declarations
        // with a `name:` pin (compose auto-creates them with the literal name).
        // `external: true` would break first-run on machines where the volume
        // has never been created.
        let mut config = base_config();
        config.extra_volumes = vec![MountSpec {
            kind: MountKind::Volume,
            source: "npm-cache".to_string(),
            target: "/home/node/.npm".to_string(),
            read_only: false,
            consistency: None,
        }];
        let yaml = generate_override_yaml(&config);
        assert!(
            yaml.contains("npm-cache:"),
            "volume declaration missing; yaml:\n{yaml}"
        );
        assert!(
            yaml.contains("    name: npm-cache"),
            "literal name pin must be emitted; yaml:\n{yaml}"
        );
        // Only the agent volume entry should be external.
        let external_count = yaml.matches("external: true").count();
        assert_eq!(
            external_count, 1,
            "must not declare extra named volumes as external — breaks first-run; yaml:\n{yaml}"
        );
    }

    #[test]
    fn named_volume_always_pins_literal_name_even_with_base_collision() {
        // Even if the base compose has a top-level key matching the source,
        // cella's override still emits its literal-name pin. Compose -f merge
        // ensures the final volume declaration carries `name: <source>`,
        // pinning the literal Docker volume name regardless of base config.
        let mut config = base_config();
        config.extra_volumes = vec![MountSpec {
            kind: MountKind::Volume,
            source: "mycache".to_string(),
            target: "/cache".to_string(),
            read_only: false,
            consistency: None,
        }];
        let yaml = generate_override_yaml(&config);
        assert!(yaml.contains("  mycache:"));
        assert!(yaml.contains("    name: mycache"));
    }

    #[test]
    fn bind_mount_does_not_emit_top_level_declaration() {
        // Bind mounts have no named volume to declare.
        let mut config = base_config();
        config.extra_volumes = vec![MountSpec::bind("/host/data", "/container/data")];
        let yaml = generate_override_yaml(&config);
        // The top-level `volumes:` block (un-indented) must contain only the
        // agent volume — no extra entry for the bind source or target path.
        // Use "\nvolumes:\n" to find the top-level section (not the service-level one).
        let top_level_volumes = yaml
            .split("\nvolumes:\n")
            .nth(1)
            .expect("must have a top-level volumes section");
        // Only "  cella-agent:" and "    external: true" should appear.
        assert_eq!(
            top_level_volumes
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count(),
            2,
            "top-level volumes must have exactly 2 non-empty lines (agent + external); yaml:\n{yaml}"
        );
    }

    #[test]
    fn tmpfs_mount_does_not_emit_top_level_declaration() {
        // Tmpfs mounts have no named volume source — no top-level declaration.
        let mut config = base_config();
        config.extra_volumes = vec![MountSpec::tmpfs("/volatile")];
        let yaml = generate_override_yaml(&config);
        // Same: only the agent volume entry.
        let top_level_volumes = yaml
            .split("\nvolumes:\n")
            .nth(1)
            .expect("must have a top-level volumes section");
        assert_eq!(
            top_level_volumes
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count(),
            2,
            "top-level volumes must have exactly 2 non-empty lines (agent + external); yaml:\n{yaml}"
        );
    }

    #[test]
    fn named_volume_source_declared_once_across_multiple_targets() {
        let mut config = base_config();
        config.extra_volumes = vec![
            MountSpec {
                kind: MountKind::Volume,
                source: "mycache".to_string(),
                target: "/cache1".to_string(),
                read_only: false,
                consistency: None,
            },
            MountSpec {
                kind: MountKind::Volume,
                source: "mycache".to_string(),
                target: "/cache2".to_string(),
                read_only: false,
                consistency: None,
            },
        ];
        let yaml = generate_override_yaml(&config);
        // Both service-level mounts present
        assert!(yaml.contains("target: \"/cache1\""));
        assert!(yaml.contains("target: \"/cache2\""));
        // Top-level volume declared ONCE
        let declarations = yaml.matches("  mycache:\n").count();
        assert_eq!(
            declarations, 1,
            "top-level volume must be declared exactly once; yaml:\n{yaml}"
        );
        let name_pins = yaml.matches("    name: mycache").count();
        assert_eq!(
            name_pins, 1,
            "literal name pin emitted exactly once; yaml:\n{yaml}"
        );
    }

    #[test]
    fn named_volume_pins_literal_docker_name() {
        // Regression: a bare top-level volume declaration (no `name:` field) is
        // project-scoped by Docker Compose as `<project>_<source>`. The user's
        // existing literal volume (which single-container `up` targets directly)
        // would then be ignored — a silent data fork. The `name:` pin preserves
        // the literal volume name regardless of the compose project name.
        let mut config = base_config();
        config.extra_volumes = vec![MountSpec {
            kind: MountKind::Volume,
            source: "shared-cache".to_string(),
            target: "/cache".to_string(),
            read_only: false,
            consistency: None,
        }];
        let yaml = generate_override_yaml(&config);
        assert!(
            yaml.contains("  shared-cache:"),
            "top-level key must be emitted; yaml:\n{yaml}"
        );
        assert!(
            yaml.contains("    name: shared-cache"),
            "literal name pin must be emitted to prevent project-scoping; yaml:\n{yaml}"
        );
        // Only the agent volume should be external.
        let external_count = yaml.matches("external: true").count();
        assert_eq!(
            external_count, 1,
            "only the agent volume should be external; yaml:\n{yaml}"
        );
    }

    // -----------------------------------------------------------------------
    // Wrapped entrypoint: resolution + emission
    // -----------------------------------------------------------------------

    #[test]
    fn no_feature_no_override_emits_no_entrypoint_or_command() {
        // The byte-for-byte preservation guarantee: with no feature entrypoints
        // and overrideCommand=false, neither key is emitted.
        let config = base_config();
        let yaml = generate_override_yaml(&config);
        assert!(!yaml.contains("entrypoint:"), "yaml:\n{yaml}");
        assert!(!yaml.contains("command:"), "yaml:\n{yaml}");
    }

    #[test]
    fn feature_entrypoints_build_wrapped_script_with_dollar_escaping() {
        // Feature entrypoints run (verbatim) between the trap and `exec "$@"`.
        // The script emits literal `$$@`/`$$!` so docker compose interpolates
        // them back to `$@`/`$!`; feature entrypoint text is NOT $-escaped.
        let mut config = base_config();
        config.feature_entrypoints = vec![
            "/usr/local/share/feat-a/entry.sh".to_string(),
            "echo from-feature-b".to_string(),
        ];
        let yaml = generate_override_yaml(&config);
        let expected = "    entrypoint: [\"/bin/sh\", \"-c\", \
             \"echo Container started\\ntrap \\\"exit 0\\\" 15\\n\
             /usr/local/share/feat-a/entry.sh\\necho from-feature-b\\n\
             exec \\\"$$@\\\"\\nwhile sleep 1 & wait $$!; do :; done\", \"-\"]\n";
        assert!(yaml.contains(expected), "yaml:\n{yaml}");
        // No agent restart loop in the compose entrypoint.
        assert!(!yaml.contains("cella-agent daemon"), "yaml:\n{yaml}");
        assert!(!yaml.contains("while true"), "yaml:\n{yaml}");
        // No `command:` when user_command is None (compose command applies).
        assert!(!yaml.contains("command:"), "yaml:\n{yaml}");
    }

    #[test]
    fn user_entrypoint_passthrough_after_dash_sentinel() {
        // userEntrypoint args are appended as JSON-quoted elements after "-",
        // preserving the service's/image's original entrypoint inside the wrap.
        // An image-derived `$$` stays doubled (already escaped at resolution).
        let mut config = base_config();
        config.feature_entrypoints = vec!["echo hi".to_string()];
        config.user_entrypoint = vec![
            "/docker-entrypoint.sh".to_string(),
            "--config=$$HOME/cfg".to_string(),
        ];
        let yaml = generate_override_yaml(&config);
        assert!(
            yaml.contains("\"-\", \"/docker-entrypoint.sh\", \"--config=$$HOME/cfg\"]\n"),
            "yaml:\n{yaml}"
        );
    }

    #[test]
    fn user_command_emitted_only_when_some() {
        // Some(cmd) -> `command:` is emitted as a JSON array; None -> omitted.
        let mut config = base_config();
        config.feature_entrypoints = vec!["echo hi".to_string()];
        config.user_command = Some(vec!["node".to_string(), "server.js".to_string()]);
        let yaml = generate_override_yaml(&config);
        assert!(
            yaml.contains("    command: [\"node\", \"server.js\"]\n"),
            "yaml:\n{yaml}"
        );

        let mut none_config = base_config();
        none_config.feature_entrypoints = vec!["echo hi".to_string()];
        none_config.user_command = None;
        let none_yaml = generate_override_yaml(&none_config);
        assert!(!none_yaml.contains("command:"), "yaml:\n{none_yaml}");
    }

    #[test]
    fn generated_wrapped_entrypoint_is_valid_yaml() {
        // The flow-sequence entrypoint (with escaped quotes/newlines and `$$`)
        // must parse as valid YAML and round-trip the script intact.
        let mut config = base_config();
        config.feature_entrypoints = vec!["echo hi".to_string()];
        config.user_entrypoint = vec!["/entry".to_string()];
        config.user_command = Some(vec!["run".to_string()]);
        let yaml = generate_override_yaml(&config);
        let parsed: yaml_serde::Value =
            yaml_serde::from_str(&yaml).expect("wrapped-entrypoint override must be valid YAML");
        let ep = &parsed["services"]["app"]["entrypoint"];
        let elems = ep.as_sequence().expect("entrypoint is a sequence");
        assert_eq!(elems[0].as_str(), Some("/bin/sh"));
        assert_eq!(elems[1].as_str(), Some("-c"));
        let script = elems[2].as_str().expect("script element is a string");
        // YAML parse resolves the `\n`/`\"` escapes; `$$` is left for compose.
        assert!(script.contains("echo Container started\ntrap \"exit 0\" 15\n"));
        assert!(script.contains("echo hi\n"));
        assert!(script.contains("exec \"$$@\"\nwhile sleep 1 & wait $$!; do :; done"));
        assert_eq!(elems[3].as_str(), Some("-"));
        assert_eq!(elems[4].as_str(), Some("/entry"));
        assert_eq!(
            parsed["services"]["app"]["command"][0].as_str(),
            Some("run")
        );
    }

    // -----------------------------------------------------------------------
    // resolve_user_entrypoint_command (parity with dockerCompose.ts)
    // -----------------------------------------------------------------------

    fn sv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn resolve_override_command_discards_to_empty() {
        // overrideCommand=true => userEntrypoint=[] and userCommand=Some([])
        // regardless of service/image values.
        let r = resolve_user_entrypoint_command(
            true,
            Some(&sv(&["/svc-entry"])),
            Some(&sv(&["svc-cmd"])),
            &sv(&["img-entry"]),
            &sv(&["img-cmd"]),
        );
        assert_eq!(r.entrypoint, Vec::<String>::new());
        assert_eq!(r.command, Some(Vec::new()));
    }

    #[test]
    fn resolve_uses_service_entrypoint_and_omits_command_when_service_has_command() {
        // composeEntrypoint present -> userEntrypoint = it (verbatim).
        // composeCommand present -> userCommand === composeCommand -> omit command.
        let r = resolve_user_entrypoint_command(
            false,
            Some(&sv(&["/svc-entry", "--x"])),
            Some(&sv(&["svc-cmd"])),
            &sv(&["img-entry"]),
            &sv(&["img-cmd"]),
        );
        assert_eq!(r.entrypoint, sv(&["/svc-entry", "--x"]));
        assert_eq!(r.command, None);
    }

    #[test]
    fn resolve_falls_back_to_image_entrypoint_and_cmd_when_service_absent() {
        // Neither service entrypoint nor command -> image entrypoint + image cmd,
        // both $-escaped, and command IS emitted (fresh array, != composeCommand).
        let r = resolve_user_entrypoint_command(
            false,
            None,
            None,
            &sv(&["/img-entry", "$VAR"]),
            &sv(&["img-cmd", "$X"]),
        );
        assert_eq!(r.entrypoint, sv(&["/img-entry", "$$VAR"]));
        assert_eq!(r.command, Some(sv(&["img-cmd", "$$X"])));
    }

    #[test]
    fn resolve_service_entrypoint_present_ignores_image_cmd() {
        // composeEntrypoint present but composeCommand absent -> image CMD is
        // ignored (compose spec) -> userCommand = Some([]) (emitted as empty).
        let r = resolve_user_entrypoint_command(
            false,
            Some(&sv(&["/svc-entry"])),
            None,
            &sv(&["img-entry"]),
            &sv(&["img-cmd"]),
        );
        assert_eq!(r.entrypoint, sv(&["/svc-entry"]));
        assert_eq!(r.command, Some(Vec::new()));
    }

    #[test]
    fn resolve_empty_service_entrypoint_is_used_not_image() {
        // An explicit empty service entrypoint (Some([])) is truthy in the
        // official `composeEntrypoint || image` -> it is used (empty), NOT the
        // image entrypoint. Image CMD is then ignored too.
        let r = resolve_user_entrypoint_command(
            false,
            Some(&[]),
            None,
            &sv(&["/img-entry"]),
            &sv(&["img-cmd"]),
        );
        assert_eq!(r.entrypoint, Vec::<String>::new());
        assert_eq!(r.command, Some(Vec::new()));
    }

    #[test]
    fn resolve_service_command_present_without_entrypoint_omits_command() {
        // composeCommand present (even without entrypoint) -> omit command;
        // userEntrypoint falls back to the image entrypoint ($-escaped).
        let r = resolve_user_entrypoint_command(
            false,
            None,
            Some(&sv(&["svc-cmd"])),
            &sv(&["/img-entry", "$Z"]),
            &sv(&["img-cmd"]),
        );
        assert_eq!(r.entrypoint, sv(&["/img-entry", "$$Z"]));
        assert_eq!(r.command, None);
    }
}
