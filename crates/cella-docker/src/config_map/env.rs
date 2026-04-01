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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_env_vars_contains_browser() {
        let vars = agent_env_vars();
        let browser = vars.iter().find(|v| v.starts_with("BROWSER="));
        assert!(browser.is_some(), "BROWSER env var must be present");
        assert!(
            browser.unwrap().contains("/cella-browser"),
            "BROWSER should point to the cella-browser helper"
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
        let val = version
            .unwrap()
            .strip_prefix("CELLA_AGENT_VERSION=")
            .unwrap();
        assert!(!val.is_empty(), "version must not be empty");
        // Version should look like a semver (contains at least one dot)
        assert!(val.contains('.'), "version should be semver-like: {val}");
    }

    #[test]
    fn agent_env_vars_returns_exactly_two() {
        let vars = agent_env_vars();
        assert_eq!(vars.len(), 2);
    }
}
