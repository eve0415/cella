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
}

impl MountKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Bind => "bind",
            Self::Tmpfs => "tmpfs",
            Self::Volume => "volume",
        }
    }

    fn from_type_str(s: &str) -> Self {
        match s {
            "tmpfs" => Self::Tmpfs,
            "volume" => Self::Volume,
            _ => Self::Bind,
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
    pub fn to_mount_config(&self) -> MountConfig {
        MountConfig {
            mount_type: self.kind.as_str().to_owned(),
            source: self.source.clone(),
            target: self.target.clone(),
            consistency: self.consistency.clone(),
        }
    }

    /// Adapter: parse from a Docker `MountConfig`.
    ///
    /// Note: `read_only` cannot be recovered (`MountConfig` doesn't carry it)
    /// — callers that need RO must set it on the spec before emission.
    pub fn from_mount_config(mc: &MountConfig) -> Self {
        Self {
            kind: MountKind::from_type_str(&mc.mount_type),
            source: mc.source.clone(),
            target: mc.target.clone(),
            read_only: false,
            consistency: mc.consistency.clone(),
        }
    }

    /// Adapter: long-form YAML entry for docker-compose override files.
    ///
    /// `indent` is the leading whitespace (e.g., `"      "` for `services.<svc>.volumes`).
    pub fn to_compose_yaml_entry(&self, indent: &str) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "{indent}- type: {}", self.kind.as_str());
        if !self.source.is_empty() {
            let _ = writeln!(out, "{indent}  source: \"{}\"", self.source);
        }
        let _ = writeln!(out, "{indent}  target: \"{}\"", self.target);
        if self.read_only {
            let _ = writeln!(out, "{indent}  read_only: true");
        }
        out
    }
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
    fn readonly_bind_mount_round_trip() {
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
        };
        let spec = MountSpec::from_mount_config(&mc);
        assert_eq!(spec.kind, MountKind::Bind);
        assert_eq!(spec.source, "/a");
        assert_eq!(spec.target, "/b");
        assert_eq!(spec.consistency.as_deref(), Some("cached"));
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
}
