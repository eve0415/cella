//! Phantom token registry for credential-protected containers.
//!
//! Stores per-container mappings between opaque phantom tokens and
//! their provider IDs, enabling the daemon to resolve which real
//! credential to inject for a given phantom token.

use std::collections::HashMap;
use std::path::PathBuf;

use tracing::{info, warn};

const PHANTOM_REGISTRY_SCHEMA_VERSION: u32 = 1;

/// Stored provider metadata for credential resolution.
#[derive(Debug, Clone)]
pub struct StoredProviderMeta {
    pub env_var: String,
    pub header: String,
    pub prefix: String,
    pub domains: Vec<String>,
}

/// Registry of phantom tokens across all credential-protected containers.
#[derive(Debug, Default)]
pub struct PhantomRegistry {
    /// `container_name -> (phantom_token -> provider_id)`
    forward: HashMap<String, HashMap<String, String>>,
    /// `container_name -> (provider_id -> phantom_token)`
    reverse: HashMap<String, HashMap<String, String>>,
    /// `container_name -> (provider_id -> env_var)`
    env_vars: HashMap<String, HashMap<String, String>>,
    /// `container_name -> (provider_id -> meta)`
    meta: HashMap<String, HashMap<String, StoredProviderMeta>>,
    /// `container_name -> nonce` (per-container authentication)
    nonces: HashMap<String, String>,
}

impl PhantomRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register phantom tokens for a container and persist to disk.
    /// Returns the per-container nonce for credential tunnel authentication.
    pub fn register(
        &mut self,
        container_name: &str,
        entries: &[cella_protocol::PhantomTokenEntry],
    ) -> String {
        self.register_without_persist(container_name, entries);
        let nonce = generate_nonce();
        self.nonces
            .insert(container_name.to_string(), nonce.clone());
        self.persist_state();
        nonce
    }

    fn register_without_persist(
        &mut self,
        container_name: &str,
        entries: &[cella_protocol::PhantomTokenEntry],
    ) {
        let mut forward = HashMap::new();
        let mut reverse = HashMap::new();
        let mut env = HashMap::new();
        let mut meta_map = HashMap::new();

        for entry in entries {
            forward.insert(entry.phantom_token.clone(), entry.provider_id.clone());
            reverse.insert(entry.provider_id.clone(), entry.phantom_token.clone());
            env.insert(entry.provider_id.clone(), entry.env_var.clone());
            meta_map.insert(
                entry.provider_id.clone(),
                StoredProviderMeta {
                    env_var: entry.env_var.clone(),
                    header: entry.header.clone(),
                    prefix: entry.prefix.clone(),
                    domains: entry.domains.clone(),
                },
            );
        }

        self.forward.insert(container_name.to_string(), forward);
        self.reverse.insert(container_name.to_string(), reverse);
        self.env_vars.insert(container_name.to_string(), env);
        self.meta.insert(container_name.to_string(), meta_map);
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

    /// Get provider metadata for a resolved provider in a container.
    pub fn get_provider_meta(
        &self,
        container_name: &str,
        provider_id: &str,
    ) -> Option<&StoredProviderMeta> {
        self.meta.get(container_name)?.get(provider_id)
    }

    /// Get registered domains for a provider in a container.
    pub fn provider_domains(&self, container_name: &str, provider_id: &str) -> Option<&[String]> {
        self.meta
            .get(container_name)?
            .get(provider_id)
            .map(|m| m.domains.as_slice())
    }

    /// Validate a per-container nonce.
    pub fn validate_nonce(&self, container_name: &str, nonce: &str) -> bool {
        self.nonces
            .get(container_name)
            .is_some_and(|stored| stored == nonce)
    }

    /// Remove all phantom tokens for a container and persist to disk.
    pub fn remove_container(&mut self, container_name: &str) {
        self.forward.remove(container_name);
        self.reverse.remove(container_name);
        self.env_vars.remove(container_name);
        self.meta.remove(container_name);
        self.nonces.remove(container_name);
        self.persist_state();
    }

    /// Number of containers with registered phantom tokens.
    pub fn container_count(&self) -> usize {
        self.forward.len()
    }

    /// Number of phantom tokens registered for a container.
    pub fn token_count(&self, container_name: &str) -> usize {
        self.forward.get(container_name).map_or(0, HashMap::len)
    }

    fn state_file_path() -> PathBuf {
        cella_env::paths::cella_data_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp/.cella"))
            .join("phantom-registry.state")
    }

    pub fn persist_state(&self) {
        let containers: serde_json::Map<String, serde_json::Value> = self
            .forward
            .keys()
            .filter_map(|container_name| {
                let forward = self.forward.get(container_name)?;
                let meta = self.meta.get(container_name)?;

                let tokens: Vec<serde_json::Value> = forward
                    .iter()
                    .filter_map(|(phantom_token, provider_id)| {
                        let m = meta.get(provider_id)?;
                        Some(serde_json::json!({
                            "provider_id": provider_id,
                            "phantom_token": phantom_token,
                            "env_var": m.env_var,
                            "domains": m.domains,
                            "header": m.header,
                            "prefix": m.prefix,
                        }))
                    })
                    .collect();

                let nonce = self.nonces.get(container_name).cloned().unwrap_or_default();
                Some((
                    container_name.clone(),
                    serde_json::json!({ "nonce": nonce, "tokens": tokens }),
                ))
            })
            .collect();

        let snapshot = serde_json::json!({
            "schema_version": PHANTOM_REGISTRY_SCHEMA_VERSION,
            "daemon_pid": std::process::id(),
            "written_at_unix_sec": crate::shared::current_time_secs(),
            "containers": containers,
        });

        let path = Self::state_file_path();
        let bytes = match serde_json::to_vec_pretty(&snapshot) {
            Ok(b) => b,
            Err(e) => {
                warn!("phantom registry: state-file serialize failed: {e}");
                return;
            }
        };

        let tmp = path.with_extension("state.tmp");

        let lock_result = acquire_state_lock(&path);
        if lock_result.is_none() {
            warn!("phantom registry: could not acquire state-file lock, proceeding without");
        }

        if let Err(e) = std::fs::write(&tmp, &bytes) {
            warn!("phantom registry: state-file write failed: {e}");
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            warn!("phantom registry: state-file rename failed: {e}");
            let _ = std::fs::remove_file(&tmp);
        }
    }

    pub fn reclaim_from_state_file(&mut self) {
        let path = Self::state_file_path();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                warn!("phantom registry: state-file read failed: {e}");
                return;
            }
        };

        let snapshot: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                warn!("phantom registry: state-file parse failed: {e}");
                return;
            }
        };

        let version = snapshot["schema_version"].as_u64().unwrap_or(0);
        if version != u64::from(PHANTOM_REGISTRY_SCHEMA_VERSION) {
            warn!("phantom registry: unknown schema version {version}, skipping");
            return;
        }

        let Some(containers) = snapshot["containers"].as_object() else {
            return;
        };

        for (container_name, data) in containers {
            let Some(tokens) = data["tokens"].as_array() else {
                continue;
            };
            let entries: Vec<cella_protocol::PhantomTokenEntry> = tokens
                .iter()
                .filter_map(|t| {
                    Some(cella_protocol::PhantomTokenEntry {
                        provider_id: t["provider_id"].as_str()?.to_string(),
                        phantom_token: t["phantom_token"].as_str()?.to_string(),
                        env_var: t["env_var"].as_str()?.to_string(),
                        domains: t["domains"]
                            .as_array()?
                            .iter()
                            .filter_map(|d| d.as_str().map(String::from))
                            .collect(),
                        header: t["header"].as_str().unwrap_or_default().to_string(),
                        prefix: t["prefix"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect();

            if !entries.is_empty() {
                self.register_without_persist(container_name, &entries);
                if let Some(nonce) = data["nonce"].as_str() {
                    self.nonces
                        .insert(container_name.clone(), nonce.to_string());
                }
            }
        }

        for container_name in self.forward.keys() {
            if !self.nonces.contains_key(container_name) {
                self.nonces.insert(container_name.clone(), generate_nonce());
            }
        }

        info!(
            "Reclaimed phantom registry: {} containers from state file",
            self.container_count()
        );
    }
}

fn acquire_state_lock(path: &std::path::Path) -> Option<std::fs::File> {
    let lock_path = path.with_extension("lock");
    let delays = [100, 200, 400];
    for delay_ms in delays {
        match std::fs::File::create(&lock_path) {
            Ok(file) => {
                if file.try_lock().is_ok() {
                    return Some(file);
                }
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
        }
    }
    None
}

fn generate_nonce() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().as_simple(),
        uuid::Uuid::new_v4().as_simple()
    )
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
                domains: vec!["api.anthropic.com".to_string()],
                header: "x-api-key".to_string(),
                prefix: String::new(),
            },
            PhantomTokenEntry {
                provider_id: "openai".to_string(),
                phantom_token: "pt-bbb".to_string(),
                env_var: "OPENAI_API_KEY".to_string(),
                domains: vec!["api.openai.com".to_string()],
                header: "Authorization".to_string(),
                prefix: "Bearer ".to_string(),
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
            domains: vec!["github.com".to_string(), "api.github.com".to_string()],
            header: "Authorization".to_string(),
            prefix: "token ".to_string(),
        }];
        reg.register("ctr-1", &new_entries);

        assert_eq!(reg.token_count("ctr-1"), 1);
        assert_eq!(reg.lookup("ctr-1", "pt-aaa"), None);
        assert_eq!(reg.lookup("ctr-1", "pt-ccc"), Some("github"));
    }
}
