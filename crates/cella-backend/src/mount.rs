//! Backend-neutral mount specification.
//!
//! A `MountSpec` describes a container mount without committing to a specific
//! runtime's representation. Two adapters convert it: [`MountSpec::to_mount_config`]
//! for the Docker API (bollard), and [`MountSpec::to_compose_yaml_entry`] for
//! docker-compose override YAML.

use crate::MountConfig;
use std::fmt::Write;

/// Kind of mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountKind {
    Bind,
    Tmpfs,
    Volume,
    /// Windows named pipe (`npipe`).
    NamedPipe,
}

impl MountKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Bind => "bind",
            Self::Tmpfs => "tmpfs",
            Self::Volume => "volume",
            Self::NamedPipe => "npipe",
        }
    }

    fn from_type_str(s: &str) -> Option<Self> {
        match s {
            "bind" => Some(Self::Bind),
            "tmpfs" => Some(Self::Tmpfs),
            "volume" => Some(Self::Volume),
            "npipe" => Some(Self::NamedPipe),
            _ => None,
        }
    }
}

/// Backend-neutral mount specification.
#[derive(Debug, Clone)]
pub struct MountSpec {
    pub kind: MountKind,
    pub source: String,
    pub target: String,
    pub read_only: bool,
    pub consistency: Option<String>,
}

impl MountSpec {
    /// Create a bind mount from `source` on the host to `target` in the container.
    pub fn bind(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            kind: MountKind::Bind,
            source: source.into(),
            target: target.into(),
            read_only: false,
            consistency: None,
        }
    }

    /// Create a tmpfs mount at `target` in the container.
    pub fn tmpfs(target: impl Into<String>) -> Self {
        Self {
            kind: MountKind::Tmpfs,
            source: String::new(),
            target: target.into(),
            read_only: false,
            consistency: None,
        }
    }

    /// Mark this mount as read-only.
    #[must_use]
    pub const fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Set an optional consistency hint (e.g., `"cached"`).
    #[must_use]
    pub fn with_consistency(mut self, c: impl Into<String>) -> Self {
        self.consistency = Some(c.into());
        self
    }

    /// Adapter: Docker API `MountConfig`.
    ///
    /// `read_only` is preserved in `MountConfig` but note that `cella-docker`
    /// does not yet forward it to bollard's `Mount` struct — wiring
    /// read-only into the single-container path is deferred to a follow-up
    /// phase.
    pub fn to_mount_config(&self) -> MountConfig {
        MountConfig {
            mount_type: self.kind.as_str().to_owned(),
            source: self.source.clone(),
            target: self.target.clone(),
            consistency: self.consistency.clone(),
            read_only: self.read_only,
        }
    }

    /// Adapter: parse from a Docker `MountConfig`.
    ///
    /// Returns `None` when `mount_type` is not a recognised kind (e.g. an
    /// unsupported or future type). Callers should log a warning and skip the
    /// mount rather than silently demoting it to `bind`.
    pub fn from_mount_config(mc: &MountConfig) -> Option<Self> {
        let kind = MountKind::from_type_str(&mc.mount_type)?;
        Some(Self {
            kind,
            source: mc.source.clone(),
            target: mc.target.clone(),
            read_only: mc.read_only,
            consistency: mc.consistency.clone(),
        })
    }

    /// Adapter: long-form YAML entry for docker-compose override files.
    ///
    /// `indent` is the leading whitespace (e.g., `"      "` for `services.<svc>.volumes`).
    pub fn to_compose_yaml_entry(&self, indent: &str) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "{indent}- type: {}", self.kind.as_str());
        if !self.source.is_empty() {
            let _ = writeln!(
                out,
                "{indent}  source: \"{}\"",
                escape_yaml_double_quoted(&self.source)
            );
        }
        let _ = writeln!(
            out,
            "{indent}  target: \"{}\"",
            escape_yaml_double_quoted(&self.target)
        );
        if self.read_only {
            let _ = writeln!(out, "{indent}  read_only: true");
        }
        if let Some(c) = &self.consistency {
            let _ = writeln!(
                out,
                "{indent}  consistency: \"{}\"",
                escape_yaml_double_quoted(c)
            );
        }
        out
    }
}

/// Escape a string for use inside a YAML double-quoted scalar.
///
/// YAML double-quoted scalars treat `\` as the escape character and `"` as the
/// closing delimiter, so both must be escaped before interpolation.
fn escape_yaml_double_quoted(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_mount_to_mount_config() {
        let spec = MountSpec::bind("/host", "/container");
        let mc = spec.to_mount_config();
        assert_eq!(mc.mount_type, "bind");
        assert_eq!(mc.source, "/host");
        assert_eq!(mc.target, "/container");
        assert_eq!(mc.consistency, None);
    }

    #[test]
    fn readonly_flag_set_on_spec() {
        let spec = MountSpec::bind("/host", "/container").read_only();
        let mc = spec.to_mount_config();
        assert_eq!(mc.mount_type, "bind");
        assert!(spec.read_only);
    }

    #[test]
    fn tmpfs_mount_has_empty_source() {
        let spec = MountSpec::tmpfs("/mnt/shadow");
        let mc = spec.to_mount_config();
        assert_eq!(mc.mount_type, "tmpfs");
        assert_eq!(mc.source, "");
        assert_eq!(mc.target, "/mnt/shadow");
    }

    #[test]
    fn from_mount_config_bind() {
        let mc = MountConfig {
            mount_type: "bind".into(),
            source: "/a".into(),
            target: "/b".into(),
            consistency: Some("cached".into()),
            read_only: false,
        };
        let spec = MountSpec::from_mount_config(&mc).unwrap();
        assert_eq!(spec.kind, MountKind::Bind);
        assert_eq!(spec.source, "/a");
        assert_eq!(spec.target, "/b");
        assert_eq!(spec.consistency.as_deref(), Some("cached"));
    }

    #[test]
    fn from_mount_config_returns_none_for_unknown_type() {
        let mc = MountConfig {
            mount_type: "cluster".into(),
            source: String::new(),
            target: "/data".into(),
            consistency: None,
            read_only: false,
        };
        assert!(
            MountSpec::from_mount_config(&mc).is_none(),
            "unknown mount type 'cluster' must yield None"
        );
    }

    #[test]
    fn from_mount_config_accepts_npipe() {
        let mc = MountConfig {
            mount_type: "npipe".into(),
            source: r"//./pipe/docker_engine".into(),
            target: r"//./pipe/docker_engine".into(),
            consistency: None,
            read_only: false,
        };
        let spec = MountSpec::from_mount_config(&mc).unwrap();
        assert_eq!(spec.kind, MountKind::NamedPipe);
        assert_eq!(spec.kind.as_str(), "npipe");
    }

    #[test]
    fn to_compose_yaml_bind_rw() {
        let spec = MountSpec::bind("/host/.claude", "/root/.claude");
        let yaml = spec.to_compose_yaml_entry("      ");
        insta::assert_snapshot!(yaml, @r#"
      - type: bind
        source: "/host/.claude"
        target: "/root/.claude"
"#);
    }

    #[test]
    fn to_compose_yaml_bind_readonly() {
        let spec = MountSpec::bind("/host/parent.git", "/host/parent.git").read_only();
        let yaml = spec.to_compose_yaml_entry("      ");
        assert!(yaml.contains("read_only: true"));
    }

    #[test]
    fn to_compose_yaml_tmpfs() {
        let spec = MountSpec::tmpfs("/root/.claude/plugins");
        let yaml = spec.to_compose_yaml_entry("      ");
        insta::assert_snapshot!(yaml, @r#"
      - type: tmpfs
        target: "/root/.claude/plugins"
"#);
    }

    #[test]
    fn to_compose_yaml_bind_with_consistency() {
        let spec = MountSpec::bind("/host", "/container").with_consistency("cached");
        let yaml = spec.to_compose_yaml_entry("      ");
        insta::assert_snapshot!(yaml, @r#"
      - type: bind
        source: "/host"
        target: "/container"
        consistency: "cached"
"#);
    }

    #[test]
    fn to_compose_yaml_escapes_backslash() {
        // Windows-style path: backslashes must be doubled in YAML double-quoted scalars.
        let spec = MountSpec::bind("C:\\Users\\a\\.codex", "/root/.codex");
        let yaml = spec.to_compose_yaml_entry("      ");
        assert!(
            yaml.contains("C:\\\\Users\\\\a\\\\.codex"),
            "expected doubled backslashes in YAML output, got:\n{yaml}"
        );
    }

    #[test]
    fn to_compose_yaml_escapes_double_quote() {
        // Path containing a double-quote must be escaped to avoid terminating the scalar.
        let spec = MountSpec::bind("/ha\"d/path", "/container");
        let yaml = spec.to_compose_yaml_entry("      ");
        assert!(
            yaml.contains("/ha\\\"d/path"),
            "expected escaped double-quote in YAML output, got:\n{yaml}"
        );
    }
}
