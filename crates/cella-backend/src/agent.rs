use tracing::{info, warn};

use crate::traits::ContainerBackend;
use crate::types::ExecOptions;

/// Environment variables injected when the managed cella-agent is available.
///
/// `wayland_clipboard` mirrors `settings.clipboard.wayland`. When on, the
/// container gets `WAYLAND_DISPLAY` pointing at the socket the agent daemon
/// serves, which is the only way clients that speak the Wayland protocol
/// directly — `arboard`, `wl-clipboard-rs` — can reach the host clipboard;
/// they never exec anything, so the `/cella/bin` shims are invisible to them.
///
/// `DISPLAY` is deliberately never set: it would activate `arboard`'s X11
/// backend, which has no server to talk to here.
pub fn agent_env_vars(wayland_clipboard: bool) -> Vec<String> {
    let version = env!("CARGO_PKG_VERSION");
    let mut vars = vec![
        "BROWSER=/cella/bin/cella-browser".to_string(),
        format!("CELLA_AGENT_VERSION={version}"),
    ];
    if wayland_clipboard {
        vars.push(format!(
            "WAYLAND_DISPLAY={}",
            cella_protocol::WAYLAND_CLIPBOARD_SOCKET
        ));
    }
    vars
}

pub async fn restart_agent_in_container(client: &dyn ContainerBackend, container_id: &str) {
    let agent_path = "/cella/bin/cella-agent";
    let script = format!(
        "pkill -f 'cella-agent daemon' 2>/dev/null; \
         sleep 1; \
         pgrep -f '[c]ella-agent daemon' >/dev/null 2>&1 || \
         \"{agent_path}\" daemon \
         --poll-interval \"${{CELLA_PORT_POLL_INTERVAL:-1000}}\" &"
    );

    match client
        .exec_detached(
            container_id,
            &ExecOptions {
                cmd: vec!["sh".to_string(), "-c".to_string(), script],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
    {
        Ok(_) => info!("Agent restart triggered in container {container_id}"),
        Err(e) => warn!("Failed to restart agent in container: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_env_vars_sets_wayland_display_when_enabled() {
        let vars = agent_env_vars(true);
        assert!(
            vars.iter().any(|v| v
                == &format!(
                    "WAYLAND_DISPLAY={}",
                    cella_protocol::WAYLAND_CLIPBOARD_SOCKET
                )),
            "got {vars:?}"
        );
    }

    #[test]
    fn agent_env_vars_omits_wayland_display_when_disabled() {
        assert!(
            !agent_env_vars(false)
                .iter()
                .any(|v| v.starts_with("WAYLAND_DISPLAY="))
        );
    }

    #[test]
    fn agent_env_vars_never_sets_display() {
        // Setting DISPLAY would send arboard down its X11 path and reintroduce
        // the "X11 server connection timed out" failure this work removes.
        for enabled in [true, false] {
            assert!(
                !agent_env_vars(enabled)
                    .iter()
                    .any(|v| v.starts_with("DISPLAY=")),
                "DISPLAY must never be set (wayland_clipboard={enabled})"
            );
        }
    }

    #[test]
    fn agent_env_vars_contains_browser() {
        let vars = agent_env_vars(true);
        let browser = vars.iter().find(|v| v.starts_with("BROWSER="));
        assert!(browser.is_some(), "BROWSER env var must be present");
        assert!(
            browser.unwrap().contains("/cella/bin/cella-browser"),
            "BROWSER should point to the managed browser helper"
        );
    }

    #[test]
    fn agent_env_vars_contains_version() {
        let vars = agent_env_vars(true);
        let version = vars.iter().find(|v| v.starts_with("CELLA_AGENT_VERSION="));
        assert!(
            version.is_some(),
            "CELLA_AGENT_VERSION env var must be present"
        );
    }
}
