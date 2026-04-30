use tracing::{info, warn};

use crate::traits::ContainerBackend;
use crate::types::ExecOptions;

/// Environment variables injected when the managed cella-agent is available.
pub fn agent_env_vars() -> Vec<String> {
    let version = env!("CARGO_PKG_VERSION");
    vec![
        "BROWSER=/cella/bin/cella-browser".to_string(),
        format!("CELLA_AGENT_VERSION={version}"),
    ]
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
    fn agent_env_vars_contains_browser() {
        let vars = agent_env_vars();
        let browser = vars.iter().find(|v| v.starts_with("BROWSER="));
        assert!(browser.is_some(), "BROWSER env var must be present");
        assert!(
            browser.unwrap().contains("/cella/bin/cella-browser"),
            "BROWSER should point to the managed browser helper"
        );
    }

    #[test]
    fn agent_env_vars_contains_version() {
        let vars = agent_env_vars();
        let version = vars.iter().find(|v| v.starts_with("CELLA_AGENT_VERSION="));
        assert!(
            version.is_some(),
            "CELLA_AGENT_VERSION env var must be present"
        );
    }
}
