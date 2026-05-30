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

/// Apply an RFC 7386 JSON Merge Patch to `base`.
///
/// A `null` value in `patch` deletes that key from `base`; nested objects merge
/// recursively; any non-object patch replaces `base` wholesale. Paired with
/// [`diff_merge_patch`], this unions the disjoint `projects` maps of the host
/// (`/Users/...`) and containers (`/workspaces/...`) while still propagating
/// deletions — unlike a pure additive merge, which cannot express a removal.
#[must_use]
pub fn apply_merge_patch(base: &serde_json::Value, patch: &serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(patch_obj) = patch else {
        return patch.clone();
    };
    let mut out = match base {
        serde_json::Value::Object(b) => b.clone(),
        _ => serde_json::Map::new(),
    };
    for (key, p) in patch_obj {
        if p.is_null() {
            out.remove(key);
        } else {
            let base_val = out.get(key).cloned().unwrap_or(serde_json::Value::Null);
            out.insert(key.clone(), apply_merge_patch(&base_val, p));
        }
    }
    serde_json::Value::Object(out)
}

/// Generate an RFC 7386 JSON Merge Patch that transforms `old` into `new`.
///
/// Keys present in `old` but absent in `new` map to `null` (deletion); added or
/// changed keys map to their new value; nested objects are diffed recursively.
/// `apply_merge_patch(old, diff_merge_patch(old, new)) == new` for inputs without
/// explicit JSON `null` values (the documented RFC 7386 limitation).
#[must_use]
pub fn diff_merge_patch(old: &serde_json::Value, new: &serde_json::Value) -> serde_json::Value {
    let (serde_json::Value::Object(old_obj), serde_json::Value::Object(new_obj)) = (old, new)
    else {
        return new.clone();
    };
    let mut patch = serde_json::Map::new();
    // Deletions: keys in `old` that `new` no longer has.
    for key in old_obj.keys() {
        if !new_obj.contains_key(key) {
            patch.insert(key.clone(), serde_json::Value::Null);
        }
    }
    // Additions and changes.
    for (key, new_val) in new_obj {
        match old_obj.get(key) {
            None => {
                patch.insert(key.clone(), new_val.clone());
            }
            Some(old_val) if old_val != new_val => {
                let sub = if old_val.is_object() && new_val.is_object() {
                    diff_merge_patch(old_val, new_val)
                } else {
                    new_val.clone()
                };
                patch.insert(key.clone(), sub);
            }
            Some(_) => {} // unchanged
        }
    }
    serde_json::Value::Object(patch)
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

    use serde_json::json;

    // ── apply_merge_patch / diff_merge_patch (RFC 7386) ──────────────────────

    #[test]
    fn apply_null_deletes_key() {
        let base = json!({ "a": 1, "b": 2 });
        assert_eq!(
            apply_merge_patch(&base, &json!({ "b": null })),
            json!({ "a": 1 })
        );
    }

    #[test]
    fn apply_null_nested_delete() {
        let base = json!({ "p": { "x": 1, "y": 2 } });
        assert_eq!(
            apply_merge_patch(&base, &json!({ "p": { "y": null } })),
            json!({ "p": { "x": 1 } })
        );
    }

    #[test]
    fn apply_adds_and_replaces_scalar() {
        let base = json!({ "a": 1 });
        assert_eq!(
            apply_merge_patch(&base, &json!({ "a": 9, "b": 2 })),
            json!({ "a": 9, "b": 2 })
        );
    }

    #[test]
    fn apply_replaces_array_wholesale() {
        // Arrays are values, not merged element-wise.
        let base = json!({ "xs": [1, 2, 3] });
        assert_eq!(
            apply_merge_patch(&base, &json!({ "xs": [9] })),
            json!({ "xs": [9] })
        );
    }

    #[test]
    fn apply_non_object_patch_replaces() {
        assert_eq!(apply_merge_patch(&json!({ "a": 1 }), &json!(5)), json!(5));
    }

    #[test]
    fn apply_object_patch_on_missing_key_strips_nested_nulls() {
        let base = json!({});
        assert_eq!(
            apply_merge_patch(&base, &json!({ "p": { "x": 1, "gone": null } })),
            json!({ "p": { "x": 1 } })
        );
    }

    #[test]
    fn diff_emits_null_for_removed_key() {
        let old = json!({ "a": 1, "b": 2 });
        let new = json!({ "a": 1 });
        assert_eq!(diff_merge_patch(&old, &new), json!({ "b": null }));
    }

    #[test]
    fn diff_added_and_changed_keys() {
        let old = json!({ "a": 1 });
        let new = json!({ "a": 2, "c": 3 });
        assert_eq!(diff_merge_patch(&old, &new), json!({ "a": 2, "c": 3 }));
    }

    #[test]
    fn diff_unchanged_is_empty_patch() {
        let v = json!({ "a": 1, "p": { "x": 1 } });
        assert_eq!(diff_merge_patch(&v, &v), json!({}));
    }

    #[test]
    fn diff_nested_only_changed_subkey() {
        let old = json!({ "p": { "x": 1, "y": 2 } });
        let new = json!({ "p": { "x": 1, "y": 9 } });
        assert_eq!(diff_merge_patch(&old, &new), json!({ "p": { "y": 9 } }));
    }

    #[test]
    fn apply_diff_roundtrips() {
        // The key property: apply(old, diff(old, new)) == new — covering
        // deletion, addition, nested change, array replace, and type changes.
        let cases = [
            (json!({ "a": 1, "b": 2 }), json!({ "a": 1 })),
            (json!({ "a": 1 }), json!({ "a": 1, "b": 2 })),
            (
                json!({ "p": { "x": 1, "y": 2 } }),
                json!({ "p": { "x": 1 } }),
            ),
            (json!({ "xs": [1, 2] }), json!({ "xs": [3] })),
            (json!({ "a": { "b": 1 } }), json!({ "a": 5 })),
            (json!({ "a": 5 }), json!({ "a": { "b": 1 } })),
            (
                json!({}),
                json!({ "mcpServers": { "s": { "command": "x" } } }),
            ),
        ];
        for (old, new) in cases {
            assert_eq!(
                apply_merge_patch(&old, &diff_merge_patch(&old, &new)),
                new,
                "roundtrip failed for old={old} new={new}"
            );
        }
    }
}
