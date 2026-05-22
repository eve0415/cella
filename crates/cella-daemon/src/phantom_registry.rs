//! Phantom token registry for credential-protected containers.
//!
//! Stores per-container mappings between opaque phantom tokens and
//! their provider IDs, enabling the daemon to resolve which real
//! credential to inject for a given phantom token.

use std::collections::HashMap;

/// Registry of phantom tokens across all credential-protected containers.
#[derive(Debug, Default)]
pub struct PhantomRegistry {
    /// `container_name -> (phantom_token -> provider_id)`
    forward: HashMap<String, HashMap<String, String>>,
    /// `container_name -> (provider_id -> phantom_token)`
    reverse: HashMap<String, HashMap<String, String>>,
    /// `container_name -> (provider_id -> env_var)`
    env_vars: HashMap<String, HashMap<String, String>>,
}

impl PhantomRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register phantom tokens for a container.
    pub fn register(
        &mut self,
        container_name: &str,
        entries: &[cella_protocol::PhantomTokenEntry],
    ) {
        let mut forward = HashMap::new();
        let mut reverse = HashMap::new();
        let mut env = HashMap::new();

        for entry in entries {
            forward.insert(entry.phantom_token.clone(), entry.provider_id.clone());
            reverse.insert(entry.provider_id.clone(), entry.phantom_token.clone());
            env.insert(entry.provider_id.clone(), entry.env_var.clone());
        }

        self.forward.insert(container_name.to_string(), forward);
        self.reverse.insert(container_name.to_string(), reverse);
        self.env_vars.insert(container_name.to_string(), env);
    }

    /// Look up a phantom token → provider ID for a container.
    pub fn lookup(&self, container_name: &str, phantom_token: &str) -> Option<&str> {
        self.forward
            .get(container_name)?
            .get(phantom_token)
            .map(String::as_str)
    }

    /// Get all `env_var -> phantom_token` pairs for a container (exec-time injection).
    pub fn get_tokens_for_container(&self, container_name: &str) -> HashMap<String, String> {
        let Some(reverse) = self.reverse.get(container_name) else {
            return HashMap::new();
        };
        let Some(env) = self.env_vars.get(container_name) else {
            return HashMap::new();
        };

        let mut result = HashMap::new();
        for (provider_id, phantom_token) in reverse {
            if let Some(env_var) = env.get(provider_id) {
                result.insert(env_var.clone(), phantom_token.clone());
            }
        }
        result
    }

    /// Remove all phantom tokens for a container.
    pub fn remove_container(&mut self, container_name: &str) {
        self.forward.remove(container_name);
        self.reverse.remove(container_name);
        self.env_vars.remove(container_name);
    }

    /// Number of containers with registered phantom tokens.
    pub fn container_count(&self) -> usize {
        self.forward.len()
    }

    /// Number of phantom tokens registered for a container.
    pub fn token_count(&self, container_name: &str) -> usize {
        self.forward.get(container_name).map_or(0, HashMap::len)
    }
}

#[cfg(test)]
mod tests {
    use cella_protocol::PhantomTokenEntry;

    use super::*;

    fn sample_entries() -> Vec<PhantomTokenEntry> {
        vec![
            PhantomTokenEntry {
                provider_id: "anthropic".to_string(),
                phantom_token: "pt-aaa".to_string(),
                env_var: "ANTHROPIC_API_KEY".to_string(),
                domain: "api.anthropic.com".to_string(),
            },
            PhantomTokenEntry {
                provider_id: "openai".to_string(),
                phantom_token: "pt-bbb".to_string(),
                env_var: "OPENAI_API_KEY".to_string(),
                domain: "api.openai.com".to_string(),
            },
        ]
    }

    #[test]
    fn register_and_lookup() {
        let mut reg = PhantomRegistry::new();
        reg.register("ctr-1", &sample_entries());

        assert_eq!(reg.lookup("ctr-1", "pt-aaa"), Some("anthropic"));
        assert_eq!(reg.lookup("ctr-1", "pt-bbb"), Some("openai"));
        assert_eq!(reg.lookup("ctr-1", "pt-unknown"), None);
    }

    #[test]
    fn lookup_unknown_container() {
        let reg = PhantomRegistry::new();
        assert_eq!(reg.lookup("nonexistent", "pt-aaa"), None);
    }

    #[test]
    fn get_tokens_for_container() {
        let mut reg = PhantomRegistry::new();
        reg.register("ctr-1", &sample_entries());

        let tokens = reg.get_tokens_for_container("ctr-1");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens["ANTHROPIC_API_KEY"], "pt-aaa");
        assert_eq!(tokens["OPENAI_API_KEY"], "pt-bbb");
    }

    #[test]
    fn get_tokens_unknown_container() {
        let reg = PhantomRegistry::new();
        let tokens = reg.get_tokens_for_container("nonexistent");
        assert!(tokens.is_empty());
    }

    #[test]
    fn remove_container() {
        let mut reg = PhantomRegistry::new();
        reg.register("ctr-1", &sample_entries());
        assert_eq!(reg.container_count(), 1);

        reg.remove_container("ctr-1");
        assert_eq!(reg.container_count(), 0);
        assert_eq!(reg.lookup("ctr-1", "pt-aaa"), None);
        assert!(reg.get_tokens_for_container("ctr-1").is_empty());
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let mut reg = PhantomRegistry::new();
        reg.remove_container("ghost");
        assert_eq!(reg.container_count(), 0);
    }

    #[test]
    fn token_count() {
        let mut reg = PhantomRegistry::new();
        reg.register("ctr-1", &sample_entries());
        assert_eq!(reg.token_count("ctr-1"), 2);
        assert_eq!(reg.token_count("nonexistent"), 0);
    }

    #[test]
    fn re_register_replaces() {
        let mut reg = PhantomRegistry::new();
        reg.register("ctr-1", &sample_entries());

        let new_entries = vec![PhantomTokenEntry {
            provider_id: "github".to_string(),
            phantom_token: "pt-ccc".to_string(),
            env_var: "GH_TOKEN".to_string(),
            domain: "api.github.com".to_string(),
        }];
        reg.register("ctr-1", &new_entries);

        assert_eq!(reg.token_count("ctr-1"), 1);
        assert_eq!(reg.lookup("ctr-1", "pt-aaa"), None);
        assert_eq!(reg.lookup("ctr-1", "pt-ccc"), Some("github"));
    }
}
