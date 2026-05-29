//! Claude Code config detection and container path helpers.
//!
//! Detects host `~/.claude/` config directory and `~/.claude.json` for
//! bind-mounting into containers. Provides path helpers for computing
//! container-side paths based on the remote user.

use std::path::PathBuf;

/// Container home path for a given user.
pub fn container_home(remote_user: &str) -> String {
    if remote_user == "root" {
        "/root".to_string()
    } else {
        format!("/home/{remote_user}")
    }
}

/// Container-side `~/.claude` directory path.
pub fn claude_dir_for_user(remote_user: &str) -> String {
    format!("{}/.claude", container_home(remote_user))
}

/// Host-side `~/.claude` directory path (if it exists).
pub fn host_claude_dir() -> Option<PathBuf> {
    let home = home_dir()?;
    let dir = home.join(".claude");
    if dir.is_dir() { Some(dir) } else { None }
}

/// Host-side `~/.claude.json` file path (if it exists).
pub fn host_claude_json_path() -> Option<PathBuf> {
    let home = home_dir()?;
    let path = home.join(".claude.json");
    if path.is_file() { Some(path) } else { None }
}

/// Host-side `~/.claude.json` path regardless of whether the file exists yet.
///
/// Unlike [`host_claude_json_path`], this does not require the file to be
/// present — the daemon's sync watcher needs the target path to watch its
/// parent directory even before Claude Code first writes the file.
pub fn host_claude_json_target() -> Option<PathBuf> {
    Some(home_dir()?.join(".claude.json"))
}

/// Host-side `~/.claude/plugins` directory path (if it exists).
pub fn host_plugins_dir() -> Option<PathBuf> {
    let dir = host_claude_dir()?.join("plugins");
    if dir.is_dir() { Some(dir) } else { None }
}

/// Host home directory derived from the host `.claude` directory path.
///
/// Returns `None` if `~/.claude/` doesn't exist on the host.
pub fn host_home() -> Option<PathBuf> {
    host_claude_dir().and_then(|d| d.parent().map(PathBuf::from))
}

/// Replace home-path prefix in file content.
///
/// Performs a simple string replacement of `{from_home}/.claude` with
/// `{to_home}/.claude` for rewriting plugin manifest paths.
pub fn rewrite_claude_home(content: &str, from_home: &str, to_home: &str) -> String {
    content.replace(
        &format!("{from_home}/.claude"),
        &format!("{to_home}/.claude"),
    )
}

/// Deep-merge `incoming` into `base`, returning the merged value.
///
/// Objects are merged key-by-key, recursing when both sides hold an object at
/// the same key. For every other case (scalars, arrays, mismatched types, a
/// non-object root) `incoming` wins. This unions the disjoint `projects` maps
/// of the host (`/Users/...`) and containers (`/workspaces/...`) for free while
/// propagating shared keys such as `oauthAccount` and `mcpServers`.
///
/// Known, accepted limitations: key *deletions* do not propagate, and two sides
/// editing the *same* scalar resolve last-writer-wins.
#[must_use]
pub fn merge_claude_config(
    base: &serde_json::Value,
    incoming: &serde_json::Value,
) -> serde_json::Value {
    match (base, incoming) {
        (serde_json::Value::Object(b), serde_json::Value::Object(i)) => {
            let mut merged = b.clone();
            for (key, incoming_val) in i {
                let entry = merged.get(key).map_or_else(
                    || incoming_val.clone(),
                    |existing| merge_claude_config(existing, incoming_val),
                );
                merged.insert(key.clone(), entry);
            }
            serde_json::Value::Object(merged)
        }
        _ => incoming.clone(),
    }
}

use crate::paths::home_dir;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_home_root() {
        assert_eq!(container_home("root"), "/root");
    }

    #[test]
    fn container_home_regular() {
        assert_eq!(container_home("vscode"), "/home/vscode");
    }

    #[test]
    fn claude_dir_for_root() {
        assert_eq!(claude_dir_for_user("root"), "/root/.claude");
    }

    #[test]
    fn claude_dir_for_regular() {
        assert_eq!(claude_dir_for_user("vscode"), "/home/vscode/.claude");
    }

    #[test]
    fn host_home_strips_claude_suffix() {
        // host_home() depends on the actual filesystem, so we test the logic
        // indirectly: if host_claude_dir() returns Some, host_home() returns its parent.
        if let Some(claude_dir) = host_claude_dir() {
            let home = host_home().expect("host_home should return Some when host_claude_dir does");
            assert_eq!(home, claude_dir.parent().unwrap());
        }
    }

    #[test]
    fn rewrite_claude_home_replaces_paths() {
        let content = r#"{"installPath": "/home/node/.claude/plugins/cache/foo"}"#;
        let result = rewrite_claude_home(content, "/home/node", "/home/vscode");
        assert_eq!(
            result,
            r#"{"installPath": "/home/vscode/.claude/plugins/cache/foo"}"#
        );
    }

    #[test]
    fn rewrite_claude_home_multiple_occurrences() {
        let content = "/home/node/.claude/a /home/node/.claude/b";
        let result = rewrite_claude_home(content, "/home/node", "/home/vscode");
        assert_eq!(result, "/home/vscode/.claude/a /home/vscode/.claude/b");
    }

    #[test]
    fn rewrite_claude_home_noop_when_same() {
        let content = "/home/vscode/.claude/plugins";
        let result = rewrite_claude_home(content, "/home/vscode", "/home/vscode");
        assert_eq!(result, content);
    }

    #[test]
    fn rewrite_claude_home_macos_to_linux() {
        let content = r#"{"path": "/Users/alice/.claude/plugins"}"#;
        let result = rewrite_claude_home(content, "/Users/alice", "/home/vscode");
        assert_eq!(result, r#"{"path": "/home/vscode/.claude/plugins"}"#);
    }

    // ── merge_claude_config ────────────────────────────────────────────────

    use serde_json::json;

    #[test]
    fn merge_unions_disjoint_projects_namespaces() {
        // The whole point of deep-merge: host keys (/Users/...) and container
        // keys (/workspaces/...) are disjoint and must both survive.
        let base = json!({
            "projects": { "/Users/eve/proj": { "allowedTools": ["a"] } }
        });
        let incoming = json!({
            "projects": { "/workspaces/proj": { "allowedTools": ["b"] } }
        });
        let merged = merge_claude_config(&base, &incoming);
        assert_eq!(
            merged,
            json!({
                "projects": {
                    "/Users/eve/proj": { "allowedTools": ["a"] },
                    "/workspaces/proj": { "allowedTools": ["b"] }
                }
            })
        );
    }

    #[test]
    fn merge_recurses_into_nested_objects() {
        let base = json!({ "a": { "b": 1 } });
        let incoming = json!({ "a": { "c": 2 } });
        let merged = merge_claude_config(&base, &incoming);
        assert_eq!(merged, json!({ "a": { "b": 1, "c": 2 } }));
    }

    #[test]
    fn merge_scalar_incoming_wins() {
        let base = json!({ "numStartups": 1, "keep": true });
        let incoming = json!({ "numStartups": 5 });
        let merged = merge_claude_config(&base, &incoming);
        assert_eq!(merged, json!({ "numStartups": 5, "keep": true }));
    }

    #[test]
    fn merge_shared_nested_scalar_last_wins() {
        let base = json!({ "projects": { "/p": { "foo": "old" } } });
        let incoming = json!({ "projects": { "/p": { "foo": "new" } } });
        let merged = merge_claude_config(&base, &incoming);
        assert_eq!(merged, json!({ "projects": { "/p": { "foo": "new" } } }));
    }

    #[test]
    fn merge_incoming_null_overwrites() {
        // null is a non-object value; incoming wins per the documented rule.
        let base = json!({ "x": 1 });
        let incoming = json!({ "x": null });
        let merged = merge_claude_config(&base, &incoming);
        assert_eq!(merged, json!({ "x": null }));
    }

    #[test]
    fn merge_preserves_base_only_keys() {
        let base = json!({ "a": 1, "b": 2 });
        let incoming = json!({ "a": 10 });
        let merged = merge_claude_config(&base, &incoming);
        assert_eq!(merged, json!({ "a": 10, "b": 2 }));
    }

    #[test]
    fn merge_empty_incoming_object_is_noop() {
        let base = json!({ "a": { "b": 1 } });
        let merged = merge_claude_config(&base, &json!({}));
        assert_eq!(merged, base);
    }

    #[test]
    fn merge_empty_base_yields_incoming() {
        let incoming = json!({ "a": 1 });
        let merged = merge_claude_config(&json!({}), &incoming);
        assert_eq!(merged, incoming);
    }

    #[test]
    fn merge_incoming_object_replaces_base_scalar_at_key() {
        let base = json!({ "a": 5 });
        let incoming = json!({ "a": { "b": 1 } });
        let merged = merge_claude_config(&base, &incoming);
        assert_eq!(merged, json!({ "a": { "b": 1 } }));
    }

    #[test]
    fn merge_incoming_scalar_replaces_base_object_at_key() {
        let base = json!({ "a": { "b": 1 } });
        let incoming = json!({ "a": 5 });
        let merged = merge_claude_config(&base, &incoming);
        assert_eq!(merged, json!({ "a": 5 }));
    }

    #[test]
    fn merge_non_object_roots_incoming_wins() {
        // Defensive: a malformed config that isn't a JSON object at the root.
        assert_eq!(merge_claude_config(&json!(1), &json!(2)), json!(2));
        assert_eq!(merge_claude_config(&json!({ "a": 1 }), &json!(7)), json!(7));
        assert_eq!(
            merge_claude_config(&json!(null), &json!({ "a": 1 })),
            json!({ "a": 1 })
        );
    }
}
