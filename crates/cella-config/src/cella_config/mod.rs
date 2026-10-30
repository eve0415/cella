pub mod cli;
pub mod error;
mod format;
mod merge;
pub mod security;

use std::path::Path;

use serde::Deserialize;

pub use cli::{Cli, CliBuild, OutputFormat, PullPolicy};
pub use error::CellaConfigError;
pub use security::{Security, SecurityMode};

use crate::devcontainer::subst::SubstitutionContext;
use crate::settings::{Clipboard, Credentials, Network, Shell, Tools};

use self::format::load_layer;
use self::merge::merge_layers;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellaConfig {
    #[serde(default)]
    pub security: Security,

    #[serde(default)]
    pub credentials: Credentials,

    #[serde(default)]
    pub tools: Tools,

    #[serde(default)]
    pub network: Network,

    #[serde(default)]
    pub shell: Shell,

    #[serde(default)]
    pub cli: Cli,

    #[serde(default)]
    pub clipboard: Clipboard,
}

impl CellaConfig {
    /// Load cella config by merging global, customizations.cella, and project layers.
    ///
    /// # Errors
    ///
    /// Returns `CellaConfigError` if any config file is malformed or contains unknown fields.
    pub fn load(
        workspace: &Path,
        resolved: Option<&crate::devcontainer::resolve::ResolvedConfig>,
    ) -> Result<Self, CellaConfigError> {
        Self::load_with_global(workspace, resolved, None)
    }

    /// Load with an explicit global config directory (for test isolation).
    ///
    /// # Errors
    ///
    /// Returns `CellaConfigError` if any config file is malformed or contains unknown fields.
    pub fn load_with_global(
        workspace: &Path,
        resolved: Option<&crate::devcontainer::resolve::ResolvedConfig>,
        global_dir: Option<&Path>,
    ) -> Result<Self, CellaConfigError> {
        // Substitute each file layer on its own, before merging: the
        // `customizations.cella` layer comes from the already-substituted
        // devcontainer config and must not be substituted a second time.
        // Relative `body_file` paths resolve against the directory of the file
        // that declared them, which is lost once the layers are merged.
        let ctx = resolved.map_or_else(
            || SubstitutionContext::local_env_only(std::env::vars().collect()),
            crate::config_map::subst_ctx,
        );
        let load_substituted =
            |dir: &Path, stem: &str| -> Result<Option<serde_json::Value>, CellaConfigError> {
                let mut layer = load_layer(dir, stem)?;
                if let Some(value) = layer.as_mut() {
                    ctx.substitute_value(value);
                    absolutize_body_files(value, dir);
                }
                Ok(layer)
            };

        let mut layers = Vec::new();

        let global = match global_dir {
            Some(dir) => load_substituted(dir, "config")?,
            None => {
                if let Some(dir) = cella_env::paths::cella_data_dir() {
                    load_substituted(&dir, "config")?
                } else {
                    None
                }
            }
        };
        if let Some(val) = global {
            layers.push(val);
        }

        if let Some(rc) = resolved
            && let Some(cella_customizations) =
                rc.config.get("customizations").and_then(|c| c.get("cella"))
        {
            reject_respond_fields(cella_customizations)?;
            layers.push(cella_customizations.clone());
        }

        let project_dir = workspace.join(".devcontainer");
        if let Some(val) = load_substituted(&project_dir, "cella")? {
            layers.push(val);
        }

        let mut merged = merge_layers(&layers);

        // shell.preferred uses replacement semantics: highest-priority layer wins
        // entirely, instead of the default array-prepend merge behavior.
        for layer in layers.iter().rev() {
            if let Some(preferred) = layer.pointer("/shell/preferred")
                && let Some(obj) = merged.pointer_mut("/shell").and_then(|v| v.as_object_mut())
            {
                obj.insert("preferred".to_string(), preferred.clone());
                break;
            }
        }

        serde_json::from_value(merged).map_err(|e| CellaConfigError::Deserialization { source: e })
    }
}

/// The `network.rules` array of a config layer, if it has one.
fn layer_rules(layer: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    layer.pointer("/network/rules")?.as_array()
}

/// Rewrite each relative `network.rules[].responses[].body_file` in a file
/// layer to an absolute path against `base` (the directory holding that
/// file), expanding a leading `~` first.
fn absolutize_body_files(layer: &mut serde_json::Value, base: &Path) {
    let Some(rules) = layer
        .pointer_mut("/network/rules")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    let body_files = rules
        .iter_mut()
        .filter_map(|rule| rule.get_mut("responses")?.as_array_mut())
        .flatten()
        .filter_map(|response| response.get_mut("body_file"));
    for body_file in body_files {
        let Some(path) = body_file.as_str() else {
            continue;
        };
        // `Path::join` returns an absolute path unchanged.
        let path = base.join(cella_env::paths::expand_tilde(path));
        *body_file = serde_json::Value::String(path.to_string_lossy().into_owned());
    }
}

/// Reject `respond` rules, `responses` and `response_headers` in the
/// `customizations.cella` layer: that layer is a merge of several files, so a
/// relative `body_file` in it has no reliable base, and cella-specific policy
/// belongs in the cella config files.
fn reject_respond_fields(layer: &serde_json::Value) -> Result<(), CellaConfigError> {
    let Some(rules) = layer_rules(layer) else {
        return Ok(());
    };
    for rule in rules {
        let Some(obj) = rule.as_object() else {
            continue;
        };
        let field = if obj.get("action").and_then(serde_json::Value::as_str) == Some("respond") {
            "\"action\": \"respond\""
        } else if obj.contains_key("responses") {
            "responses"
        } else if obj.contains_key("response_headers") {
            "response_headers"
        } else {
            continue;
        };
        let domain = obj
            .get("domain")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<no domain>");
        let paths: Vec<String> = obj
            .get("paths")
            .and_then(serde_json::Value::as_array)
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        return Err(CellaConfigError::RespondFieldInCustomizations {
            rule: cella_network::config::rule_label(domain, &paths),
            field: field.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tempfile::TempDir;

    fn make_resolved(config: Value) -> crate::devcontainer::resolve::ResolvedConfig {
        let typed = crate::schema::DevContainer::validate(&config, "").ok();
        crate::devcontainer::resolve::ResolvedConfig {
            config,
            config_path: std::path::PathBuf::new(),
            workspace_root: std::path::PathBuf::new(),
            config_hash: String::new(),
            devcontainer_id: String::new(),
            warnings: vec![],
            typed,
            raw_remote_env: None,
            raw_config: Value::Null,
            container_identity: None,
            default_container_workspace_folder: None,
        }
    }

    #[test]
    fn no_config_files_returns_defaults() {
        let tmp = TempDir::new().unwrap();
        let cfg = CellaConfig::load_with_global(tmp.path(), None, Some(tmp.path())).unwrap();
        assert_eq!(cfg.security.mode, SecurityMode::Disabled);
        assert!(cfg.credentials.gh);
        assert!(cfg.tools.claude_code.forward_config);
    }

    #[test]
    fn global_toml_loaded() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.toml"),
            "[security]\nmode = \"enforced\"\n",
        )
        .unwrap();
        let cfg = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap();
        assert_eq!(cfg.security.mode, SecurityMode::Enforced);
    }

    #[test]
    fn project_overrides_global() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.toml"),
            "[security]\nmode = \"logged\"\n\n[credentials]\ngh = true\n",
        )
        .unwrap();
        let devcontainer = ws.path().join(".devcontainer");
        std::fs::create_dir_all(&devcontainer).unwrap();
        std::fs::write(
            devcontainer.join("cella.toml"),
            "[security]\nmode = \"enforced\"\n",
        )
        .unwrap();
        let cfg = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap();
        assert_eq!(cfg.security.mode, SecurityMode::Enforced);
        assert!(cfg.credentials.gh);
    }

    #[test]
    fn customizations_cella_extracted() {
        let ws = TempDir::new().unwrap();
        let global = TempDir::new().unwrap();
        let resolved = make_resolved(json!({
            "customizations": {
                "cella": {
                    "security": {"mode": "logged"}
                }
            }
        }));
        let cfg =
            CellaConfig::load_with_global(ws.path(), Some(&resolved), Some(global.path())).unwrap();
        assert_eq!(cfg.security.mode, SecurityMode::Logged);
    }

    #[test]
    fn three_layer_precedence() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.toml"),
            "[security]\nmode = \"disabled\"\n",
        )
        .unwrap();
        let resolved = make_resolved(json!({
            "customizations": {
                "cella": {
                    "security": {"mode": "logged"}
                }
            }
        }));
        let devcontainer = ws.path().join(".devcontainer");
        std::fs::create_dir_all(&devcontainer).unwrap();
        std::fs::write(
            devcontainer.join("cella.toml"),
            "[security]\nmode = \"enforced\"\n",
        )
        .unwrap();
        let cfg =
            CellaConfig::load_with_global(ws.path(), Some(&resolved), Some(global.path())).unwrap();
        assert_eq!(cfg.security.mode, SecurityMode::Enforced);
    }

    #[test]
    fn invalid_toml_hard_fails() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(global.path().join("config.toml"), "not valid {{{").unwrap();
        let err = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap_err();
        assert!(matches!(err, CellaConfigError::ParseToml { .. }));
    }

    #[test]
    fn deny_unknown_fields_rejects_typos() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.toml"),
            "[securityy]\nmode = \"enforced\"\n",
        )
        .unwrap();
        let err = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap_err();
        assert!(matches!(err, CellaConfigError::Deserialization { .. }));
    }

    #[test]
    fn toml_preferred_over_json() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.toml"),
            "[security]\nmode = \"enforced\"\n",
        )
        .unwrap();
        std::fs::write(
            global.path().join("config.json"),
            r#"{"security": {"mode": "logged"}}"#,
        )
        .unwrap();
        let cfg = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap();
        assert_eq!(cfg.security.mode, SecurityMode::Enforced);
    }

    #[test]
    fn jsonc_comments_in_json() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.json"),
            r#"{
                // comment
                "security": {
                    "mode": "logged" /* inline */
                }
            }"#,
        )
        .unwrap();
        let cfg = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap();
        assert_eq!(cfg.security.mode, SecurityMode::Logged);
    }

    #[test]
    fn missing_customizations_skipped() {
        let ws = TempDir::new().unwrap();
        let global = TempDir::new().unwrap();
        let resolved = make_resolved(json!({"image": "ubuntu"}));
        let cfg =
            CellaConfig::load_with_global(ws.path(), Some(&resolved), Some(global.path())).unwrap();
        assert_eq!(cfg.security.mode, SecurityMode::Disabled);
    }

    #[test]
    fn deep_merge_preserves_sibling_fields() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.toml"),
            "[credentials]\ngh = false\n\n[credentials.ai]\nenabled = true\nopenai = false\n",
        )
        .unwrap();
        let devcontainer = ws.path().join(".devcontainer");
        std::fs::create_dir_all(&devcontainer).unwrap();
        std::fs::write(
            devcontainer.join("cella.toml"),
            "[credentials.ai]\nanthropic = false\n",
        )
        .unwrap();
        let cfg = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap();
        assert!(!cfg.credentials.gh);
        assert!(cfg.credentials.ai.enabled);
        assert!(!cfg.credentials.ai.is_provider_enabled("openai"));
        assert!(!cfg.credentials.ai.is_provider_enabled("anthropic"));
    }

    #[test]
    fn shell_preferred_replacement_semantics() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.toml"),
            "[shell]\npreferred = [\"zsh\", \"bash\", \"sh\"]\n",
        )
        .unwrap();
        let devcontainer = ws.path().join(".devcontainer");
        std::fs::create_dir_all(&devcontainer).unwrap();
        std::fs::write(
            devcontainer.join("cella.toml"),
            "[shell]\npreferred = [\"bash\", \"sh\"]\n",
        )
        .unwrap();
        let cfg = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap();
        assert_eq!(cfg.shell.preferred, vec!["bash", "sh"]);
    }

    #[test]
    fn shell_preferred_from_customizations() {
        let ws = TempDir::new().unwrap();
        let global = TempDir::new().unwrap();
        let resolved = make_resolved(json!({
            "customizations": {
                "cella": {
                    "shell": {"preferred": ["fish", "zsh", "bash", "sh"]}
                }
            }
        }));
        let cfg =
            CellaConfig::load_with_global(ws.path(), Some(&resolved), Some(global.path())).unwrap();
        assert_eq!(cfg.shell.preferred, vec!["fish", "zsh", "bash", "sh"]);
    }

    #[test]
    fn file_layers_expand_local_env_without_resolved_config() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.toml"),
            concat!(
                "[network.proxy]\nca_cert = \"${localEnv",
                ":HOME}/ca.pem\"\n"
            ),
        )
        .unwrap();
        let cfg = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap();
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(
            cfg.network.proxy.ca_cert.as_deref(),
            Some(format!("{home}/ca.pem").as_str())
        );
    }

    #[test]
    fn file_layers_keep_workspace_variables_literal_without_resolved_config() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let devcontainer = ws.path().join(".devcontainer");
        std::fs::create_dir_all(&devcontainer).unwrap();
        std::fs::write(
            devcontainer.join("cella.json"),
            concat!(
                r#"{"network": {"proxy": {"#,
                r#""ca_cert": "${localWorkspaceFolder}/ca.pem","#,
                r#""http": "${devcontainerId}","#,
                r#""no_proxy": "${containerEnv"#,
                r#":NO_PROXY}"}}}"#,
            ),
        )
        .unwrap();
        let cfg = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap();
        assert_eq!(
            cfg.network.proxy.ca_cert.as_deref(),
            Some("${localWorkspaceFolder}/ca.pem")
        );
        assert_eq!(cfg.network.proxy.http.as_deref(), Some("${devcontainerId}"));
        assert_eq!(
            cfg.network.proxy.no_proxy.as_deref(),
            Some(concat!("${containerEnv", ":NO_PROXY}"))
        );
    }

    #[test]
    fn file_layers_use_resolved_config_context() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let devcontainer = ws.path().join(".devcontainer");
        std::fs::create_dir_all(&devcontainer).unwrap();
        std::fs::write(
            devcontainer.join("cella.toml"),
            "[network.proxy]\nca_cert = \"${localWorkspaceFolderBasename}.pem\"\n",
        )
        .unwrap();
        let mut resolved = make_resolved(json!({"image": "ubuntu"}));
        resolved.workspace_root = ws.path().to_path_buf();
        let cfg =
            CellaConfig::load_with_global(ws.path(), Some(&resolved), Some(global.path())).unwrap();
        let basename = ws
            .path()
            .canonicalize()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(
            cfg.network.proxy.ca_cert.as_deref(),
            Some(format!("{basename}.pem").as_str())
        );
    }

    /// A resolved config whose paths point into `ws`, as `cella up` builds it.
    fn make_resolved_at(config: Value, ws: &Path) -> crate::devcontainer::resolve::ResolvedConfig {
        let mut resolved = make_resolved(config);
        resolved.config_path = ws.join(".devcontainer").join("devcontainer.json");
        resolved.workspace_root = ws.to_path_buf();
        resolved
    }

    const RESPOND_RULE_TOML: &str = r#"
[[network.rules]]
domain = "api.example.com"
paths = ["/v1/messages"]
action = "respond"

[[network.rules.responses]]
status = 200
body_file = "fixtures/message.json"
"#;

    fn first_body_file(cfg: &CellaConfig) -> &Path {
        cfg.network.rules[0].responses[0]
            .body_file
            .as_deref()
            .unwrap()
    }

    #[test]
    fn project_body_file_resolves_against_devcontainer_dir() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let devcontainer = ws.path().join(".devcontainer");
        std::fs::create_dir_all(devcontainer.join("fixtures")).unwrap();
        std::fs::write(devcontainer.join("cella.toml"), RESPOND_RULE_TOML).unwrap();
        std::fs::write(devcontainer.join("fixtures/message.json"), "{}").unwrap();
        let resolved = make_resolved_at(json!({"image": "ubuntu"}), ws.path());
        let cfg =
            CellaConfig::load_with_global(ws.path(), Some(&resolved), Some(global.path())).unwrap();
        assert_eq!(
            first_body_file(&cfg),
            devcontainer.join("fixtures/message.json")
        );
        let net = cfg
            .network
            .to_network_config(cella_network::NetworkRulePolicy::Enforce)
            .unwrap();
        assert_eq!(net.rules[0].responses[0].body, "{}");
    }

    #[test]
    fn global_body_file_resolves_against_global_dir() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(global.path().join("config.toml"), RESPOND_RULE_TOML).unwrap();
        let resolved = make_resolved_at(json!({"image": "ubuntu"}), ws.path());
        let cfg =
            CellaConfig::load_with_global(ws.path(), Some(&resolved), Some(global.path())).unwrap();
        assert_eq!(
            first_body_file(&cfg),
            global.path().join("fixtures/message.json")
        );
    }

    #[test]
    fn absolute_body_file_is_kept() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.toml"),
            RESPOND_RULE_TOML.replace("fixtures/message.json", "/srv/fixtures/message.json"),
        )
        .unwrap();
        let cfg = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap();
        assert_eq!(
            first_body_file(&cfg),
            Path::new("/srv/fixtures/message.json")
        );
    }

    #[test]
    fn tilde_body_file_expands_to_home() {
        let Some(home) = cella_env::paths::home_dir() else {
            return;
        };
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.toml"),
            RESPOND_RULE_TOML.replace("fixtures/message.json", "~/fixtures/message.json"),
        )
        .unwrap();
        let cfg = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap();
        assert_eq!(first_body_file(&cfg), home.join("fixtures/message.json"));
    }

    #[test]
    fn body_file_is_substituted_before_it_is_made_absolute() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let devcontainer = ws.path().join(".devcontainer");
        std::fs::create_dir_all(&devcontainer).unwrap();
        std::fs::write(
            devcontainer.join("cella.toml"),
            RESPOND_RULE_TOML.replace(
                "fixtures/message.json",
                "${localWorkspaceFolder}/fixtures/message.json",
            ),
        )
        .unwrap();
        let resolved = make_resolved_at(json!({"image": "ubuntu"}), ws.path());
        let cfg =
            CellaConfig::load_with_global(ws.path(), Some(&resolved), Some(global.path())).unwrap();
        let body_file = first_body_file(&cfg);
        assert!(body_file.is_absolute(), "{}", body_file.display());
        assert!(body_file.ends_with("fixtures/message.json"));
        assert!(
            !body_file.starts_with(&devcontainer),
            "an already-absolute substituted path must not be joined to .devcontainer: {}",
            body_file.display()
        );
    }

    #[test]
    fn respond_fields_in_customizations_are_rejected() {
        let rules = [
            json!({"domain": "api.example.com", "paths": ["/v1/messages"], "action": "respond"}),
            json!({"domain": "api.example.com", "action": "block", "responses": [{"status": 200}]}),
            json!({"domain": "api.example.com", "action": "allow", "response_headers": {"x-a": "1"}}),
        ];
        for (rule, field) in
            rules
                .into_iter()
                .zip(["\"action\": \"respond\"", "responses", "response_headers"])
        {
            let ws = TempDir::new().unwrap();
            let global = TempDir::new().unwrap();
            let resolved = make_resolved_at(
                json!({"customizations": {"cella": {"network": {"rules": [rule]}}}}),
                ws.path(),
            );
            let err =
                CellaConfig::load_with_global(ws.path(), Some(&resolved), Some(global.path()))
                    .unwrap_err();
            assert!(
                matches!(
                    err,
                    CellaConfigError::RespondFieldInCustomizations { field: ref f, .. } if f == field
                ),
                "{field}: {err}"
            );
            assert!(err.to_string().contains(".devcontainer/cella.toml"));
        }
    }

    #[test]
    fn plain_rules_in_customizations_are_accepted() {
        let ws = TempDir::new().unwrap();
        let global = TempDir::new().unwrap();
        let resolved = make_resolved_at(
            json!({"customizations": {"cella": {"network": {"rules": [
                {"domain": "api.example.com", "action": "block"}
            ]}}}}),
            ws.path(),
        );
        let cfg =
            CellaConfig::load_with_global(ws.path(), Some(&resolved), Some(global.path())).unwrap();
        assert_eq!(cfg.network.rules.len(), 1);
    }

    #[test]
    fn no_shell_config_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let cfg = CellaConfig::load_with_global(tmp.path(), None, Some(tmp.path())).unwrap();
        assert!(cfg.shell.preferred.is_empty());
    }
}
