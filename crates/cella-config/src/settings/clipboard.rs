use serde::Deserialize;

const fn default_true() -> bool {
    true
}

/// Host clipboard integration settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clipboard {
    /// Serve a Wayland clipboard socket in the container and set
    /// `WAYLAND_DISPLAY` (default: true).
    ///
    /// Turn this off if a GUI toolkit in your container misbehaves when it
    /// believes a compositor is present — cella advertises only clipboard
    /// globals, not `wl_compositor`/`wl_shm`. The `/cella/bin` shims
    /// (`xclip`, `xsel`, `wl-copy`, `wl-paste`) keep working either way.
    #[serde(default = "default_true")]
    pub wayland: bool,
}

impl Default for Clipboard {
    fn default() -> Self {
        Self { wayland: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_enabled() {
        assert!(Clipboard::default().wayland);
    }

    #[test]
    fn deserialize_empty_uses_defaults() {
        let settings: Clipboard = toml::from_str("").unwrap();
        assert!(settings.wayland);
    }

    #[test]
    fn deserialize_wayland_disabled() {
        let settings: Clipboard = toml::from_str("wayland = false").unwrap();
        assert!(!settings.wayland);
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(toml::from_str::<Clipboard>("enabled = true").is_err());
    }
}
