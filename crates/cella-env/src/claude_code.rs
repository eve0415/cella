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

/// Manifest fields carrying an absolute filesystem path.
///
/// Rewriting is restricted to these rather than applied to the raw document text
/// so a prefix appearing inside an unrelated string can never be corrupted.
const PATH_FIELDS: &[&str] = &["installPath", "installLocation", "projectPath"];

/// Bidirectional path mapping between one container's view and the host's.
///
/// Claude Code string-prefix-matches `installPath` / `installLocation`, so the
/// manifest each side reads must carry paths literally correct for that side.
/// `workspace` is `None` when the container's workspace is not bind-mounted from
/// a host directory, in which case `projectPath` passes through.
#[derive(Debug, Clone)]
pub struct PathMap {
    /// `({container_home}/.claude, {host_home}/.claude)`
    pub claude: (String, String),
    /// `({container_workspace}, {host_workspace})`
    pub workspace: Option<(String, String)>,
}

/// Which way a rewrite runs.
#[derive(Debug, Clone, Copy)]
enum Direction {
    ToHost,
    ToContainer,
}

impl PathMap {
    /// Container-shaped document → host-shaped.
    #[must_use]
    pub fn to_host(&self, doc: &serde_json::Value) -> serde_json::Value {
        self.rewritten(doc, Direction::ToHost)
    }

    /// Host-shaped document → container-shaped.
    #[must_use]
    pub fn to_container(&self, doc: &serde_json::Value) -> serde_json::Value {
        self.rewritten(doc, Direction::ToContainer)
    }

    /// Apply the first matching prefix substitution to every [`PATH_FIELDS`]
    /// value.
    ///
    /// Pairs are tried **longest prefix first**. The two mappings are disjoint on
    /// a typical layout, but nothing guarantees it — a workspace living under the
    /// `.claude` directory would otherwise match the shorter `.claude` pair and
    /// be mangled, and the mangling is not symmetric, so
    /// `to_container(to_host(x)) != x`. Longest-first makes the more specific
    /// mapping win in both directions, which restores the inverse property.
    fn rewritten(&self, doc: &serde_json::Value, direction: Direction) -> serde_json::Value {
        let mut subs: Vec<(&str, &str)> = std::iter::once(&self.claude)
            .chain(self.workspace.iter())
            .map(|(container, host)| match direction {
                Direction::ToHost => (container.as_str(), host.as_str()),
                Direction::ToContainer => (host.as_str(), container.as_str()),
            })
            .collect();
        subs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        let mut out = doc.clone();
        rewrite_path_fields(&mut out, &subs);
        out
    }
}

/// Walk `value`, replacing a leading `from` with `to` in every path field.
fn rewrite_path_fields(value: &mut serde_json::Value, subs: &[(&str, &str)]) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if PATH_FIELDS.contains(&key.as_str())
                    && let Some(text) = child.as_str()
                    && let Some((from, to)) = subs.iter().find(|(from, _)| text.starts_with(from))
                {
                    *child = serde_json::Value::String(text.replacen(from, to, 1));
                    continue;
                }
                rewrite_path_fields(child, subs);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                rewrite_path_fields(item, subs);
            }
        }
        _ => {}
    }
}

/// Key an `installed_plugins.json` entry by its install context.
///
/// `scope` alone is ambiguous once a plugin is installed for more than one
/// project, so a non-user scope is qualified by its `projectPath`.
fn entry_context_key(entry: &serde_json::Value) -> String {
    let scope = entry
        .get("scope")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("user");
    entry
        .get("projectPath")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| scope.to_string(), |path| format!("{scope}:{path}"))
}

/// Rewrite each `plugins[key]` entry array into an object keyed by install
/// context.
///
/// RFC 7386 replaces arrays wholesale, so leaving the entry lists as arrays
/// would make two containers editing different scopes of the same plugin
/// clobber each other. This form exists only in memory and on the wire —
/// [`denormalize_installed_plugins`] restores the on-disk schema before any
/// write.
///
/// A `plugins` value that is not an array is passed through unchanged rather
/// than dropped: an unrecognised shape from a newer Claude Code must survive a
/// round trip, not be silently deleted.
#[must_use]
pub fn normalize_installed_plugins(doc: &serde_json::Value) -> serde_json::Value {
    let mut out = doc.clone();
    let Some(plugins) = out
        .get_mut("plugins")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return out;
    };
    for value in plugins.values_mut() {
        let Some(entries) = value.as_array() else {
            continue;
        };
        let mut keyed = serde_json::Map::new();
        for entry in entries {
            keyed.insert(entry_context_key(entry), entry.clone());
        }
        *value = serde_json::Value::Object(keyed);
    }
    out
}

/// Restore the on-disk entry-array schema from the normalized form.
///
/// Entries are emitted in sorted context-key order so the output is byte-stable:
/// loop suppression hashes raw bytes, and an unstable order would read as a
/// change on every write.
#[must_use]
pub fn denormalize_installed_plugins(doc: &serde_json::Value) -> serde_json::Value {
    let mut out = doc.clone();
    let Some(plugins) = out
        .get_mut("plugins")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return out;
    };
    for value in plugins.values_mut() {
        let Some(keyed) = value.as_object() else {
            continue;
        };
        let mut keys: Vec<&String> = keyed.keys().collect();
        keys.sort();
        let entries: Vec<serde_json::Value> = keys
            .into_iter()
            .filter_map(|k| keyed.get(k).cloned())
            .collect();
        *value = serde_json::Value::Array(entries);
    }
    out
}

/// On-disk content → canonical (host-shaped, normalized) form.
///
/// `map` is `None` on the host, where the on-disk form is already host-shaped.
/// It is also ignored for [`SyncDoc::ClaudeJson`], whose codec is the identity:
/// host and container `projects` keys are disjoint and merge additively, so that
/// document is never path-translated regardless of what the caller passes.
///
/// Returns `None` for unparseable input so the caller can skip the event rather
/// than merge a fabricated empty document over real state.
#[must_use]
pub fn to_canonical(doc: SyncDoc, raw: &str, map: Option<&PathMap>) -> Option<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_str(raw).ok()?;
    Some(match doc {
        SyncDoc::ClaudeJson => parsed,
        SyncDoc::KnownMarketplaces => map.map_or_else(|| parsed.clone(), |m| m.to_host(&parsed)),
        SyncDoc::InstalledPlugins => {
            let hosted = map.map_or_else(|| parsed.clone(), |m| m.to_host(&parsed));
            normalize_installed_plugins(&hosted)
        }
    })
}

/// Canonical form → on-disk content for whichever side `map` describes.
#[must_use]
pub fn to_local(doc: SyncDoc, canonical: &serde_json::Value, map: Option<&PathMap>) -> String {
    let localized = match doc {
        SyncDoc::ClaudeJson => canonical.clone(),
        SyncDoc::KnownMarketplaces => {
            map.map_or_else(|| canonical.clone(), |m| m.to_container(canonical))
        }
        SyncDoc::InstalledPlugins => {
            let denormalized = denormalize_installed_plugins(canonical);
            map.map_or_else(|| denormalized.clone(), |m| m.to_container(&denormalized))
        }
    };
    serde_json::to_string_pretty(&localized).unwrap_or_else(|_| "{}".to_string())
}

use crate::paths::home_dir;
use cella_protocol::SyncDoc;

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

    // ── installed_plugins.json normalization ────────────────────────────────

    #[test]
    fn normalize_keys_entries_by_scope_and_project() {
        let doc = json!({
            "version": 2,
            "plugins": {
                "p@m": [
                    { "scope": "user", "installPath": "/h/.claude/plugins/cache/p" },
                    { "scope": "project", "projectPath": "/w/a", "installPath": "/h/.claude/plugins/cache/p" }
                ]
            }
        });
        let n = normalize_installed_plugins(&doc);
        assert_eq!(n["version"], json!(2));
        let entries = &n["plugins"]["p@m"];
        assert!(entries.get("user").is_some());
        assert!(entries.get("project:/w/a").is_some());
        assert_eq!(
            entries["user"]["installPath"],
            json!("/h/.claude/plugins/cache/p")
        );
    }

    /// The merge granularity this normalization exists for: two sources editing
    /// different install contexts of the same plugin must not clobber each other,
    /// which a wholesale-replaced array cannot express.
    #[test]
    fn normalized_form_merges_per_context() {
        let base = normalize_installed_plugins(&json!({
            "version": 2,
            "plugins": { "p@m": [
                { "scope": "user", "version": "1.0" },
                { "scope": "project", "projectPath": "/w/a", "version": "1.0" }
            ]}
        }));
        let theirs = normalize_installed_plugins(&json!({
            "version": 2,
            "plugins": { "p@m": [
                { "scope": "user", "version": "2.0" },
                { "scope": "project", "projectPath": "/w/a", "version": "1.0" }
            ]}
        }));
        let patch = diff_merge_patch(&base, &theirs);
        let merged = apply_merge_patch(&base, &patch);
        assert_eq!(merged["plugins"]["p@m"]["user"]["version"], json!("2.0"));
        assert_eq!(
            merged["plugins"]["p@m"]["project:/w/a"]["version"],
            json!("1.0")
        );
    }

    #[test]
    fn denormalize_roundtrips_with_sorted_entries() {
        let doc = json!({
            "version": 2,
            "plugins": { "p@m": [
                { "scope": "user", "version": "1.0" },
                { "scope": "project", "projectPath": "/w/a", "version": "1.0" }
            ]}
        });
        let back = denormalize_installed_plugins(&normalize_installed_plugins(&doc));
        let entries = back["plugins"]["p@m"].as_array().expect("array restored");
        assert_eq!(entries.len(), 2);
        // Sorted by context key: "project:/w/a" < "user".
        assert_eq!(entries[0]["scope"], json!("project"));
        assert_eq!(entries[1]["scope"], json!("user"));
    }

    #[test]
    fn normalize_leaves_unexpected_shapes_untouched() {
        let doc = json!({ "version": 2, "plugins": { "p@m": "not-an-array" } });
        assert_eq!(normalize_installed_plugins(&doc), doc);
    }

    // ── PathMap ─────────────────────────────────────────────────────────────

    fn test_map() -> PathMap {
        PathMap {
            claude: ("/home/vscode/.claude".into(), "/Users/alice/.claude".into()),
            workspace: Some(("/workspaces/cella".into(), "/Users/alice/src/cella".into())),
        }
    }

    #[test]
    fn to_host_rewrites_install_and_project_paths() {
        let doc = json!({ "plugins": { "p@m": [{
            "installPath": "/home/vscode/.claude/plugins/cache/p",
            "projectPath": "/workspaces/cella",
            "version": "1.0"
        }]}});
        let out = test_map().to_host(&doc);
        let e = &out["plugins"]["p@m"][0];
        assert_eq!(
            e["installPath"],
            json!("/Users/alice/.claude/plugins/cache/p")
        );
        assert_eq!(e["projectPath"], json!("/Users/alice/src/cella"));
        assert_eq!(e["version"], json!("1.0"));
    }

    #[test]
    fn to_container_is_the_inverse() {
        let container =
            json!({ "m": { "installLocation": "/home/vscode/.claude/plugins/marketplaces/m" }});
        let host =
            json!({ "m": { "installLocation": "/Users/alice/.claude/plugins/marketplaces/m" }});
        let map = test_map();
        assert_eq!(map.to_host(&container), host);
        assert_eq!(map.to_container(&host), container);
    }

    /// A non-path field that happens to contain a matching substring must not be
    /// rewritten — the reason this targets fields rather than raw text.
    #[test]
    fn non_path_fields_are_untouched() {
        let doc = json!({ "note": "installed under /home/vscode/.claude/plugins" });
        assert_eq!(test_map().to_host(&doc), doc);
    }

    /// Nothing guarantees the two mappings are disjoint. When the workspace sits
    /// under the claude directory, the shorter `.claude` prefix must not win —
    /// its rewrite is not symmetric, so first-match-wins would break the inverse.
    #[test]
    fn nested_workspace_still_roundtrips() {
        let map = PathMap {
            claude: ("/home/vscode/.claude".into(), "/Users/alice/.claude".into()),
            workspace: Some((
                "/home/vscode/.claude/work".into(),
                "/Users/alice/.claude/work".into(),
            )),
        };
        let doc = json!({ "p": [{
            "projectPath": "/home/vscode/.claude/work/cella",
            "installPath": "/home/vscode/.claude/plugins/cache/p"
        }]});
        let hosted = map.to_host(&doc);
        assert_eq!(
            hosted["p"][0]["projectPath"],
            json!("/Users/alice/.claude/work/cella")
        );
        assert_eq!(
            hosted["p"][0]["installPath"],
            json!("/Users/alice/.claude/plugins/cache/p")
        );
        assert_eq!(map.to_container(&hosted), doc);
    }

    #[test]
    fn unmapped_workspace_leaves_project_path_alone() {
        let map = PathMap {
            claude: ("/home/vscode/.claude".into(), "/Users/alice/.claude".into()),
            workspace: None,
        };
        let doc = json!({ "p": [{ "projectPath": "/workspaces/other" }] });
        assert_eq!(map.to_host(&doc), doc);
    }

    // ── document codec ──────────────────────────────────────────────────────

    #[test]
    fn claude_json_codec_is_identity() {
        let raw = r#"{"projects":{"/workspaces/x":{}}}"#;
        let canon = to_canonical(SyncDoc::ClaudeJson, raw, None).expect("parses");
        assert_eq!(
            canon,
            serde_json::from_str::<serde_json::Value>(raw).expect("valid json")
        );
    }

    /// `~/.claude.json` never gets path translation: host and container
    /// `projects` keys are disjoint and merge additively, so a map passed by a
    /// caller that syncs all three documents must be ignored for this one.
    #[test]
    fn claude_json_codec_ignores_the_path_map() {
        let raw = r#"{"projects":{"/workspaces/x":{"projectPath":"/workspaces/cella"}}}"#;
        let canon = to_canonical(SyncDoc::ClaudeJson, raw, Some(&test_map())).expect("parses");
        assert_eq!(
            canon,
            serde_json::from_str::<serde_json::Value>(raw).expect("valid json")
        );
        assert_eq!(
            to_local(SyncDoc::ClaudeJson, &canon, Some(&test_map())),
            serde_json::to_string_pretty(&canon).expect("serializes")
        );
    }

    #[test]
    fn installed_plugins_codec_roundtrips_through_container_form() {
        let on_disk = r#"{"version":2,"plugins":{"p@m":[{"scope":"user","installPath":"/home/vscode/.claude/plugins/cache/p"}]}}"#;
        let map = test_map();
        let canon = to_canonical(SyncDoc::InstalledPlugins, on_disk, Some(&map)).expect("parses");
        // Canonical is host-shaped and normalized.
        assert_eq!(
            canon["plugins"]["p@m"]["user"]["installPath"],
            json!("/Users/alice/.claude/plugins/cache/p")
        );
        let local = to_local(SyncDoc::InstalledPlugins, &canon, Some(&map));
        let reparsed: serde_json::Value = serde_json::from_str(&local).expect("valid json");
        assert_eq!(
            reparsed,
            serde_json::from_str::<serde_json::Value>(on_disk).expect("valid json")
        );
    }

    #[test]
    fn invalid_json_yields_none_rather_than_a_default() {
        assert!(to_canonical(SyncDoc::KnownMarketplaces, "{not json", None).is_none());
    }
}
