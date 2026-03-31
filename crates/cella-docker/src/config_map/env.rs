/// Generate agent-related environment variables.
///
/// These are injected into the container to support the cella-agent:
/// - `BROWSER`: points to the cella-browser helper script
/// - `CELLA_AGENT_VERSION`: agent version for self-identification
/// - `CELLA_PORT_POLL_INTERVAL`: configurable poll interval (default 1000ms)
pub fn agent_env_vars() -> Vec<String> {
    let browser_path = crate::volume::browser_helper_path();
    let version = env!("CARGO_PKG_VERSION");

    vec![
        format!("BROWSER={browser_path}"),
        format!("CELLA_AGENT_VERSION={version}"),
    ]
}
