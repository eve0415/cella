/// Environment variables injected when the managed cella-agent is available.
pub fn agent_env_vars() -> Vec<String> {
    let version = env!("CARGO_PKG_VERSION");
    vec![
        "BROWSER=/cella/bin/cella-browser".to_string(),
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
