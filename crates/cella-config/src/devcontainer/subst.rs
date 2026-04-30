//! Devcontainer spec variable substitution.
//!
//! Resolves `${localEnv:VAR}`, `${containerEnv:VAR}`, `${localWorkspaceFolder}`,
//! `${containerWorkspaceFolder}`, `${localWorkspaceFolderBasename}`, and
//! `${devcontainerId}` expressions in configuration values.

use std::collections::HashMap;
use std::path::Path;

/// Context for resolving devcontainer variable expressions.
#[derive(Clone)]
pub struct SubstitutionContext {
    local_env: HashMap<String, String>,
    local_workspace_folder: String,
    local_workspace_folder_basename: String,
    container_workspace_folder: String,
    devcontainer_id: String,
}

impl SubstitutionContext {
    /// Create a new substitution context.
    ///
    /// `env` is injectable for testing; use `std::env::vars().collect()` in production.
    pub fn new(
        workspace_root: &Path,
        container_workspace_folder: Option<&str>,
        devcontainer_id: &str,
        env: HashMap<String, String>,
    ) -> Self {
        let canonical = workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.to_path_buf());
        let folder = canonical.to_string_lossy().to_string();
        let basename = canonical
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().to_string());
        let container_wf = container_workspace_folder
            .map_or_else(|| format!("/workspaces/{basename}"), String::from);

        Self {
            local_env: env,
            local_workspace_folder: folder,
            local_workspace_folder_basename: basename,
            container_workspace_folder: container_wf,
            devcontainer_id: devcontainer_id.to_string(),
        }
    }

    /// Substitute all `${...}` expressions in a string.
    pub fn substitute_str(&self, input: &str) -> String {
        let mut result = String::with_capacity(input.len());
        let mut rest = input;

        while let Some(start) = rest.find("${") {
            result.push_str(&rest[..start]);
            let after_open = &rest[start + 2..];

            if let Some(close) = after_open.find('}') {
                let expr = &after_open[..close];
                result.push_str(&self.resolve_expr(expr));
                rest = &after_open[close + 1..];
            } else {
                // No matching '}' — pass through literally
                result.push_str("${");
                rest = after_open;
            }
        }

        result.push_str(rest);
        result
    }

    /// Recursively substitute string values in a JSON tree.
    ///
    /// Walks arrays and object values. Does NOT substitute object keys.
    pub fn substitute_value(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(s) => {
                let substituted = self.substitute_str(s);
                if *s != substituted {
                    *s = substituted;
                }
            }
            serde_json::Value::Array(arr) => {
                for item in arr {
                    self.substitute_value(item);
                }
            }
            serde_json::Value::Object(map) => {
                for val in map.values_mut() {
                    self.substitute_value(val);
                }
            }
            _ => {}
        }
    }

    /// Resolve a single expression (the content between `${` and `}`).
    fn resolve_expr(&self, expr: &str) -> String {
        // Split into at most 3 parts: keyword, name, default
        let mut parts = expr.splitn(3, ':');
        let keyword = parts.next().unwrap_or("");

        match keyword {
            "localEnv" => {
                let var_name = parts.next().unwrap_or("");
                let default = parts.next().unwrap_or("");
                self.local_env
                    .get(var_name)
                    .cloned()
                    .unwrap_or_else(|| default.to_string())
            }
            "containerEnv" => {
                // Container not running at resolve time — use default or empty
                parts.next(); // advance past var name to reach default
                let default = parts.next().unwrap_or("");
                default.to_string()
            }
            "localWorkspaceFolder" => self.local_workspace_folder.clone(),
            "containerWorkspaceFolder" => self.container_workspace_folder.clone(),
            "localWorkspaceFolderBasename" => self.local_workspace_folder_basename.clone(),
            "containerWorkspaceFolderBasename" => {
                // Extract the last path component from the container workspace folder
                Path::new(&self.container_workspace_folder)
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().to_string())
            }
            "devcontainerId" => self.devcontainer_id.clone(),
            _ => format!("${{{expr}}}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> SubstitutionContext {
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), "/home/testuser".to_string());
        env.insert("EMPTY_VAR".to_string(), String::new());

        SubstitutionContext {
            local_env: env,
            local_workspace_folder: "/projects/myapp".to_string(),
            local_workspace_folder_basename: "myapp".to_string(),
            container_workspace_folder: "/workspaces/myapp".to_string(),
            devcontainer_id: "abc123".to_string(),
        }
    }

    #[test]
    fn local_env_resolves() {
        let ctx = test_ctx();
        assert_eq!(ctx.substitute_str("${localEnv:HOME}"), "/home/testuser");
    }

    #[test]
    fn local_env_missing_is_empty() {
        let ctx = test_ctx();
        assert_eq!(ctx.substitute_str("${localEnv:NONEXISTENT}"), "");
    }

    #[test]
    fn local_env_missing_uses_default() {
        let ctx = test_ctx();
        assert_eq!(
            ctx.substitute_str("${localEnv:NONEXISTENT:fallback}"),
            "fallback"
        );
    }

    #[test]
    fn local_env_present_ignores_default() {
        let ctx = test_ctx();
        assert_eq!(
            ctx.substitute_str("${localEnv:HOME:fallback}"),
            "/home/testuser"
        );
    }

    #[test]
    fn local_env_empty_value_not_default() {
        let ctx = test_ctx();
        assert_eq!(ctx.substitute_str("${localEnv:EMPTY_VAR:fallback}"), "");
    }

    #[test]
    fn container_env_is_empty() {
        let ctx = test_ctx();
        assert_eq!(ctx.substitute_str("${containerEnv:PATH}"), "");
    }

    #[test]
    fn container_env_uses_default() {
        let ctx = test_ctx();
        assert_eq!(
            ctx.substitute_str("${containerEnv:SHELL:default}"),
            "default"
        );
    }

    #[test]
    fn workspace_variables() {
        let ctx = test_ctx();
        assert_eq!(
            ctx.substitute_str("${localWorkspaceFolder}"),
            "/projects/myapp"
        );
        assert_eq!(
            ctx.substitute_str("${containerWorkspaceFolder}"),
            "/workspaces/myapp"
        );
        assert_eq!(
            ctx.substitute_str("${localWorkspaceFolderBasename}"),
            "myapp"
        );
        assert_eq!(ctx.substitute_str("${devcontainerId}"), "abc123");
    }

    #[test]
    fn multiple_variables_in_one_string() {
        let ctx = test_ctx();
        let input = concat!(
            "source=${localEnv:HOME}",
            "/.claude.json,target=/home/vscode/.claude.json"
        );
        assert_eq!(
            ctx.substitute_str(input),
            "source=/home/testuser/.claude.json,target=/home/vscode/.claude.json"
        );
    }

    #[test]
    fn no_variables_unchanged() {
        let ctx = test_ctx();
        assert_eq!(ctx.substitute_str("plain text"), "plain text");
    }

    #[test]
    fn malformed_no_closing_brace() {
        let ctx = test_ctx();
        assert_eq!(ctx.substitute_str("${localEnv:HOME"), "${localEnv:HOME");
    }

    #[test]
    fn unrecognized_variable_passed_through() {
        let ctx = test_ctx();
        assert_eq!(ctx.substitute_str("${unknownVar:foo}"), "${unknownVar:foo}");
    }

    #[test]
    fn substitute_value_walks_json() {
        let ctx = test_ctx();
        let mount_entry = concat!(
            "source=${localEnv:HOME}",
            "/.config,target=/home/user/.config"
        );
        let workspace_var = concat!("${containerWorkspaceFolder", "}");
        let env_var = concat!("${localEnv:HOME", "}");
        let mut value = serde_json::json!({
            "mounts": [
                mount_entry,
                "plain"
            ],
            "workspaceFolder": workspace_var,
            "nested": {
                "env": env_var
            },
            "number": 42,
            "bool": true
        });

        ctx.substitute_value(&mut value);

        assert_eq!(
            value["mounts"][0],
            "source=/home/testuser/.config,target=/home/user/.config"
        );
        assert_eq!(value["mounts"][1], "plain");
        assert_eq!(value["workspaceFolder"], "/workspaces/myapp");
        assert_eq!(value["nested"]["env"], "/home/testuser");
        assert_eq!(value["number"], 42);
        assert_eq!(value["bool"], true);
    }

    #[test]
    fn substitute_value_skips_object_keys() {
        let ctx = test_ctx();
        let mut value = serde_json::json!({
            "${localEnv:HOME}": "value"
        });

        ctx.substitute_value(&mut value);

        // Key should remain unchanged
        assert!(value.get("${localEnv:HOME}").is_some());
        assert_eq!(value["${localEnv:HOME}"], "value");
    }

    #[test]
    fn default_with_colons_preserved() {
        let ctx = test_ctx();
        assert_eq!(
            ctx.substitute_str("${localEnv:NONEXISTENT:/usr/bin:/usr/local/bin}"),
            "/usr/bin:/usr/local/bin"
        );
    }

    #[test]
    fn constructor_computes_fields() {
        let tmp = std::env::temp_dir().join("test_subst_workspace");
        std::fs::create_dir_all(&tmp).unwrap();

        let mut env = HashMap::new();
        env.insert("HOME".to_string(), "/home/test".to_string());

        let ctx = SubstitutionContext::new(&tmp, None, "deadbeef", env);

        assert!(!ctx.local_workspace_folder.is_empty());
        assert_eq!(ctx.local_workspace_folder_basename, "test_subst_workspace");
        assert_eq!(
            ctx.container_workspace_folder,
            "/workspaces/test_subst_workspace"
        );
        assert_eq!(ctx.devcontainer_id, "deadbeef");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn constructor_uses_explicit_container_workspace() {
        let tmp = std::env::temp_dir().join("test_subst_explicit");
        std::fs::create_dir_all(&tmp).unwrap();

        let ctx = SubstitutionContext::new(&tmp, Some("/custom/path"), "id1", HashMap::new());
        assert_eq!(ctx.container_workspace_folder, "/custom/path");

        std::fs::remove_dir_all(&tmp).ok();
    }

    // --- Spec compliance tests ---
    // Reference: https://containers.dev/implementors/json_reference/#variables-in-devcontainerjson

    fn spec_ctx_with_env(env: HashMap<String, String>) -> SubstitutionContext {
        let tmp = std::env::temp_dir().join("spec_test_ws");
        std::fs::create_dir_all(&tmp).ok();
        SubstitutionContext::new(
            &tmp,
            Some("/workspaces/myproject"),
            "test-devcontainer-id-52chars00000000000000000000000",
            env,
        )
    }

    fn spec_default_ctx() -> SubstitutionContext {
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), "/home/user".to_string());
        env.insert("PATH".to_string(), "/usr/bin:/usr/local/bin".to_string());
        env.insert("EMPTY".to_string(), String::new());
        spec_ctx_with_env(env)
    }

    #[test]
    fn spec_local_env_resolves_to_host_var() {
        let ctx = spec_default_ctx();
        assert_eq!(ctx.substitute_str("${localEnv:HOME}"), "/home/user");
    }

    #[test]
    fn spec_local_env_missing_resolves_to_empty() {
        let ctx = spec_default_ctx();
        assert_eq!(ctx.substitute_str("${localEnv:NONEXISTENT}"), "");
    }

    #[test]
    fn spec_local_env_uses_default_when_missing() {
        let ctx = spec_default_ctx();
        assert_eq!(
            ctx.substitute_str("${localEnv:NONEXISTENT:fallback}"),
            "fallback"
        );
    }

    #[test]
    fn spec_local_env_ignores_default_when_present() {
        let ctx = spec_default_ctx();
        assert_eq!(
            ctx.substitute_str("${localEnv:HOME:fallback}"),
            "/home/user"
        );
    }

    #[test]
    fn spec_local_env_empty_value_is_not_default() {
        let ctx = spec_default_ctx();
        assert_eq!(ctx.substitute_str("${localEnv:EMPTY:fallback}"), "");
    }

    #[test]
    fn spec_local_env_default_with_colons() {
        let ctx = spec_default_ctx();
        assert_eq!(
            ctx.substitute_str("${localEnv:NONEXISTENT:/usr/bin:/usr/local/bin}"),
            "/usr/bin:/usr/local/bin"
        );
    }

    #[test]
    fn spec_container_env_resolves_to_empty_at_config_time() {
        let ctx = spec_default_ctx();
        assert_eq!(ctx.substitute_str("${containerEnv:PATH}"), "");
    }

    #[test]
    fn spec_container_env_uses_default_at_config_time() {
        let ctx = spec_default_ctx();
        assert_eq!(
            ctx.substitute_str("${containerEnv:SHELL:/bin/bash}"),
            "/bin/bash"
        );
    }

    #[test]
    fn spec_local_workspace_folder_resolves() {
        let ctx = spec_default_ctx();
        let result = ctx.substitute_str("${localWorkspaceFolder}");
        assert!(!result.is_empty());
        assert!(!result.contains("${"));
    }

    #[test]
    fn spec_container_workspace_folder_resolves() {
        let ctx = spec_default_ctx();
        assert_eq!(
            ctx.substitute_str("${containerWorkspaceFolder}"),
            "/workspaces/myproject"
        );
    }

    #[test]
    fn spec_local_workspace_folder_basename_resolves() {
        let ctx = spec_default_ctx();
        let result = ctx.substitute_str("${localWorkspaceFolderBasename}");
        assert!(!result.is_empty());
        assert!(!result.contains('/'));
    }

    #[test]
    fn spec_container_workspace_folder_basename_resolves() {
        let ctx = spec_default_ctx();
        assert_eq!(
            ctx.substitute_str("${containerWorkspaceFolderBasename}"),
            "myproject"
        );
    }

    #[test]
    fn spec_devcontainer_id_resolves() {
        let ctx = spec_default_ctx();
        let result = ctx.substitute_str("${devcontainerId}");
        assert!(!result.is_empty());
        assert!(!result.contains("${"));
    }

    #[test]
    fn spec_multiple_variables_in_one_string() {
        let ctx = spec_default_ctx();
        let input = concat!(
            "source=${localEnv:HOME}/.config,",
            "target=${containerWorkspaceFolder}/.config"
        );
        let result = ctx.substitute_str(input);
        assert_eq!(
            result,
            "source=/home/user/.config,target=/workspaces/myproject/.config"
        );
    }

    #[test]
    fn spec_container_env_substitution_works_syntactically() {
        let ctx = spec_default_ctx();
        let result = ctx.substitute_str("${containerEnv:SOME_VAR:default_value}");
        assert_eq!(result, "default_value");
    }

    #[test]
    fn substituted_values_not_rescanned() {
        let mut env = HashMap::new();
        env.insert("TRICKY".to_string(), "${localWorkspaceFolder}".to_string());
        let ctx = SubstitutionContext::new(Path::new("/tmp/ws"), None, "id123", env);
        assert_eq!(
            ctx.substitute_str("${localEnv:TRICKY}"),
            "${localWorkspaceFolder}"
        );
    }
}
