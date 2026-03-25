//! Claude Code config detection, path rewriting, and container injection.
//!
//! Detects host `~/.claude/` config, filters files per include/exclude rules,
//! rewrites hardcoded home paths for cross-user portability, and prepares
//! uploads for container injection.

use std::path::{Path, PathBuf};

use regex::Regex;

use crate::FileUpload;

/// Files always copied from `~/.claude/` root.
const DEFAULT_COPY_FILES: &[&str] = &[".credentials.json", "settings.json", "CLAUDE.md"];

/// Directories always copied recursively from `~/.claude/`.
const DEFAULT_COPY_DIRS: &[&str] = &["commands", "plugins", "hooks", "rules"];

/// Files always excluded.
const DEFAULT_EXCLUDE_FILES: &[&str] = &["history.jsonl"];

/// Subdirectory names skipped when recursively walking `DEFAULT_COPY_DIRS`.
/// These contain machine-generated, re-downloadable content.
const WALK_SKIP_DIRS: &[&str] = &["cache"];

/// Text file extensions that receive path rewriting.
const REWRITE_EXTENSIONS: &[&str] = &["json", "jsonl", "md", "sh", "toml", "yml", "yaml", "txt"];

/// File extensions treated as executable in the heuristic fallback.
const EXECUTABLE_EXTENSIONS: &[&str] = &["sh", "bash", "zsh", "py", "rb", "pl"];

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
fn host_claude_dir() -> Option<PathBuf> {
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

/// Get the host home directory.
fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE").ok().map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}

/// Command to check if Claude config already exists in a container.
pub fn claude_config_exists_command(claude_dir: &str) -> Vec<String> {
    vec!["test".to_string(), "-d".to_string(), claude_dir.to_string()]
}

/// Prepare Claude Code config files for container injection (first create).
///
/// Reads `~/.claude/`, filters per include/exclude rules, rewrites hardcoded
/// home paths to match the container user, and returns upload-ready files.
///
/// Returns `None` if `~/.claude/` doesn't exist on the host.
pub fn prepare_claude_config(
    remote_user: &str,
    workspace_root: &Path,
    settings: &cella_config::ClaudeCode,
) -> Option<Vec<FileUpload>> {
    let host_dir = host_claude_dir()?;
    let target_home = container_home(remote_user);
    let target_claude_dir = claude_dir_for_user(remote_user);
    let rewrites = build_path_rewrites(&target_home);

    let mut uploads = Vec::new();
    let mut collector = Collector {
        host_dir: &host_dir,
        target_claude_dir: &target_claude_dir,
        rewrites: &rewrites,
        uploads: &mut uploads,
    };

    // 1. Copy default root files
    for &file in DEFAULT_COPY_FILES {
        if is_user_excluded(file, &settings.exclude) {
            continue;
        }
        collector.file(file);
    }

    // 2. Copy user-created root files (*.sh, *.md, etc. not in exclude list)
    collector.user_root_files(settings);

    // 3. Copy default directories
    for &dir in DEFAULT_COPY_DIRS {
        if is_user_excluded(dir, &settings.exclude) {
            continue;
        }
        collector.directory(dir);
    }

    // 4. Copy matching projects/ subdirectory for current workspace
    collector.workspace_project(workspace_root);

    // 5. Apply user include patterns (for files/dirs beyond defaults)
    for pattern in &settings.include {
        collector.glob(pattern);
    }

    if uploads.is_empty() {
        None
    } else {
        Some(uploads)
    }
}

/// Prepare auth-only re-sync for subsequent starts.
///
/// Returns just `.credentials.json` for re-upload.
pub fn prepare_claude_auth_resync(remote_user: &str) -> Option<Vec<FileUpload>> {
    let host_dir = host_claude_dir()?;
    let target_claude_dir = claude_dir_for_user(remote_user);

    let cred_path = host_dir.join(".credentials.json");
    let content = std::fs::read(&cred_path).ok()?;

    Some(vec![FileUpload {
        container_path: format!("{target_claude_dir}/.credentials.json"),
        content,
        mode: 0o600,
    }])
}

// ---------------------------------------------------------------------------
// Path rewriting
// ---------------------------------------------------------------------------

/// Build regex patterns for all known home-path variants.
fn build_path_rewrites(target_home: &str) -> Vec<(Regex, String)> {
    let target = format!("{target_home}/.claude");
    vec![
        // /home/USERNAME/.claude
        (
            Regex::new(r#"/home/[^/\s"']+/\.claude"#).expect("valid regex"),
            target.clone(),
        ),
        // C:/Users/USERNAME/.claude (Windows forward-slash) — must be before macOS
        (
            Regex::new(r#"[A-Z]:/Users/[^/\s"']+/\.claude"#).expect("valid regex"),
            target.clone(),
        ),
        // /Users/USERNAME/.claude (macOS) — after Windows to avoid partial match
        (
            Regex::new(r#"/Users/[^/\s"']+/\.claude"#).expect("valid regex"),
            target.clone(),
        ),
        // C:\\Users\\USERNAME\\.claude (JSON-escaped Windows backslash)
        (
            Regex::new(r#"[A-Z]:\\\\Users\\\\[^\\\\"'\s]+\\\\\.claude"#).expect("valid regex"),
            target,
        ),
    ]
}

/// Rewrite home-like `.claude` paths in UTF-8 content.
///
/// Returns `Some(rewritten)` if any replacements were made, `None` otherwise.
fn rewrite_paths(content: &str, rewrites: &[(Regex, String)]) -> Option<String> {
    let mut result = content.to_string();
    let mut changed = false;

    for (pattern, replacement) in rewrites {
        let new = pattern.replace_all(&result, replacement.as_str());
        if new != result {
            changed = true;
            result = new.into_owned();
        }
    }

    // Plain string replacement for /root/.claude (avoids trivial regex).
    // The target replacement is the same as all regex replacements above.
    if let Some((_, target)) = rewrites.first() {
        let new = result.replace("/root/.claude", target);
        if new != result {
            changed = true;
            result = new;
        }
    }

    if changed { Some(result) } else { None }
}

/// Apply path rewriting to file content if the extension is a known text type.
fn maybe_rewrite(content: &[u8], relative_path: &str, rewrites: &[(Regex, String)]) -> Vec<u8> {
    if !should_rewrite(relative_path) {
        return content.to_vec();
    }
    let Ok(text) = std::str::from_utf8(content) else {
        return content.to_vec();
    };
    rewrite_paths(text, rewrites).map_or_else(|| content.to_vec(), String::into_bytes)
}

/// Check if a file should receive path rewriting based on extension.
fn should_rewrite(path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("");
    REWRITE_EXTENSIONS.contains(&ext)
}

// ---------------------------------------------------------------------------
// File collection
// ---------------------------------------------------------------------------

/// Collects host `.claude/` files into container-ready [`FileUpload`]s.
///
/// Bundles the shared state (`host_dir`, `target_claude_dir`, `rewrites`,
/// `uploads`) so every collection method can focus on its own filtering logic.
struct Collector<'c> {
    host_dir: &'c Path,
    target_claude_dir: &'c str,
    rewrites: &'c [(Regex, String)],
    uploads: &'c mut Vec<FileUpload>,
}

/// Check if a file should be treated as sensitive (capped at 0o600).
///
/// Matches hidden files with `.json` extension (e.g., `.credentials.json`).
fn is_sensitive_file(relative_path: &str) -> bool {
    relative_path.starts_with('.')
        && relative_path
            .rsplit('.')
            .next()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

/// Determine file mode from extension or shebang when host metadata is unavailable.
fn heuristic_file_mode(content: &[u8], relative_path: &str) -> u32 {
    if let Some(ext) = relative_path.rsplit('.').next()
        && EXECUTABLE_EXTENSIONS
            .iter()
            .any(|e| e.eq_ignore_ascii_case(ext))
    {
        return 0o755;
    }

    // Check for shebang only in extensionless files.
    if !relative_path.contains('.') && content.starts_with(b"#!") {
        return 0o755;
    }

    0o644
}

/// Resolve the file mode for a host file being uploaded to the container.
///
/// Priority:
/// 1. Sensitive files (hidden + `.json`) are capped at 0o600.
/// 2. On Unix, reads the host file's actual permission bits.
/// 3. Falls back to extension/shebang heuristics when metadata is unavailable.
fn resolve_file_mode(host_path: &Path, content: &[u8], relative_path: &str) -> u32 {
    if is_sensitive_file(relative_path) {
        tracing::debug!(
            path = relative_path,
            mode = "0o600",
            source = "sensitive-cap",
            "Resolved file mode"
        );
        return 0o600;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = host_path.metadata() {
            let host_mode = metadata.permissions().mode() & 0o777;
            tracing::debug!(
                path = relative_path,
                mode = format_args!("{host_mode:#o}").to_string(),
                source = "host",
                "Resolved file mode"
            );
            return host_mode;
        }
    }

    let mode = heuristic_file_mode(content, relative_path);
    tracing::debug!(
        path = relative_path,
        mode = format_args!("{mode:#o}").to_string(),
        source = "heuristic",
        "Resolved file mode"
    );
    mode
}

impl Collector<'_> {
    // -- public collection methods ------------------------------------------

    /// Collect a single file from the host `.claude/` directory.
    fn file(&mut self, relative_path: &str) {
        let host_path = self.host_dir.join(relative_path);
        let Ok(content) = std::fs::read(&host_path) else {
            return;
        };
        let mode = resolve_file_mode(&host_path, &content, relative_path);
        self.push_file(relative_path, &content, mode);
    }

    /// Collect all files in a directory recursively.
    fn directory(&mut self, relative_dir: &str) {
        let dir_path = self.host_dir.join(relative_dir);
        if dir_path.is_dir() {
            self.walk_dir(&dir_path);
        }
    }

    /// Collect user-created files at the root of `~/.claude/` (not in subdirectories).
    ///
    /// Includes files like `statusline-command.sh` that the user created,
    /// excluding known machine-generated files and directories.
    fn user_root_files(&mut self, settings: &cella_config::ClaudeCode) {
        let Ok(entries) = std::fs::read_dir(self.host_dir) else {
            return;
        };

        let already_copied: Vec<&str> = DEFAULT_COPY_FILES.to_vec();

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            // Skip files already in the default copy list
            if already_copied.contains(&name) {
                continue;
            }

            // Skip default exclude files
            if DEFAULT_EXCLUDE_FILES.contains(&name) {
                continue;
            }

            // Skip user-excluded files
            if is_user_excluded(name, &settings.exclude) {
                continue;
            }

            // Skip known machine-generated files
            if name.starts_with("security_warnings_state_") {
                continue;
            }

            let Ok(content) = std::fs::read(&path) else {
                continue;
            };
            let mode = resolve_file_mode(&path, &content, name);
            self.push_file(name, &content, mode);
        }
    }

    /// Collect the `projects/` subdirectory matching the current workspace.
    ///
    /// Claude Code encodes workspace paths by replacing `/` with `-`.
    /// e.g., `/workspaces/cella` -> `projects/-workspaces-cella/`
    fn workspace_project(&mut self, workspace_root: &Path) {
        let workspace_str = workspace_root.to_string_lossy();
        let encoded = workspace_str.replace('/', "-");
        let project_dir = self.host_dir.join("projects").join(&encoded);

        if project_dir.is_dir() {
            let relative_prefix = format!("projects/{encoded}");
            self.walk_dir(&project_dir);
            tracing::debug!("Collected project config from {relative_prefix}");
        }
    }

    /// Collect files matching a user-provided glob pattern.
    fn glob(&mut self, pattern: &str) {
        let full_pattern = format!("{}/{pattern}", self.host_dir.display());
        let Ok(paths) = glob::glob(&full_pattern) else {
            tracing::warn!("Invalid glob pattern: {pattern}");
            return;
        };

        for entry in paths.flatten() {
            if !entry.is_file() {
                continue;
            }
            let relative = match entry.strip_prefix(self.host_dir) {
                Ok(r) => r.to_string_lossy().to_string(),
                Err(_) => continue,
            };

            // Skip if already in uploads (avoid duplicates)
            if self
                .uploads
                .iter()
                .any(|u| u.container_path.ends_with(&relative))
            {
                continue;
            }

            let Ok(content) = std::fs::read(&entry) else {
                continue;
            };
            let mode = resolve_file_mode(&entry, &content, &relative);
            self.push_file(&relative, &content, mode);
        }
    }

    // -- private helpers ----------------------------------------------------

    /// Read, rewrite, and push a single file into the uploads vec.
    fn push_file(&mut self, relative_path: &str, content: &[u8], mode: u32) {
        let content = maybe_rewrite(content, relative_path, self.rewrites);
        self.uploads.push(FileUpload {
            container_path: format!("{}/{relative_path}", self.target_claude_dir),
            content,
            mode,
        });
    }

    /// Recursively walk a directory and collect files.
    fn walk_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str())
                    && WALK_SKIP_DIRS.contains(&name)
                {
                    continue;
                }
                self.walk_dir(&path);
            } else if path.is_file() {
                let relative = match path.strip_prefix(self.host_dir) {
                    Ok(r) => r.to_string_lossy().to_string(),
                    Err(_) => continue,
                };
                let Ok(content) = std::fs::read(&path) else {
                    continue;
                };
                let mode = resolve_file_mode(&path, &content, &relative);
                self.push_file(&relative, &content, mode);
            }
        }
    }
}

/// Check if a path matches any user-provided exclude pattern.
///
/// Matches both the path directly and as a prefix. For example,
/// pattern `"plans/**"` excludes both the `plans` directory and its contents.
fn is_user_excluded(path: &str, exclude_patterns: &[String]) -> bool {
    for pattern in exclude_patterns {
        if let Ok(glob_pattern) = glob::Pattern::new(pattern)
            && glob_pattern.matches(path)
        {
            return true;
        }
        // Also check if the pattern starts with the path as a directory prefix.
        // e.g., pattern "plans/**" should exclude "plans" itself.
        if let Some(dir_prefix) = pattern.strip_suffix("/**")
            && path == dir_prefix
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- container_home ---

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

    // --- path rewriting ---

    #[test]
    fn rewrite_linux_home_path() {
        let rewrites = build_path_rewrites("/home/vscode");
        let input = r#"{"installPath": "/home/node/.claude/plugins/cache/foo"}"#;
        let result = rewrite_paths(input, &rewrites).unwrap();
        assert_eq!(
            result,
            r#"{"installPath": "/home/vscode/.claude/plugins/cache/foo"}"#
        );
    }

    #[test]
    fn rewrite_macos_path() {
        let rewrites = build_path_rewrites("/home/vscode");
        let input = r#"{"path": "/Users/alice/.claude/settings.json"}"#;
        let result = rewrite_paths(input, &rewrites).unwrap();
        assert_eq!(result, r#"{"path": "/home/vscode/.claude/settings.json"}"#);
    }

    #[test]
    fn rewrite_root_path() {
        let rewrites = build_path_rewrites("/home/vscode");
        let input = r#""/root/.claude/plugins""#;
        let result = rewrite_paths(input, &rewrites).unwrap();
        assert_eq!(result, r#""/home/vscode/.claude/plugins""#);
    }

    #[test]
    fn rewrite_windows_forward_slash() {
        let rewrites = build_path_rewrites("/home/vscode");
        let input = r#"{"path": "C:/Users/bob/.claude/settings"}"#;
        let result = rewrite_paths(input, &rewrites).unwrap();
        assert_eq!(result, r#"{"path": "/home/vscode/.claude/settings"}"#);
    }

    #[test]
    fn rewrite_windows_escaped_backslash() {
        let rewrites = build_path_rewrites("/home/vscode");
        let input = r#"{"path": "C:\\Users\\bob\\.claude\\plugins"}"#;
        let result = rewrite_paths(input, &rewrites).unwrap();
        assert_eq!(result, r#"{"path": "/home/vscode/.claude\\plugins"}"#);
    }

    #[test]
    fn rewrite_multiple_paths_same_file() {
        let rewrites = build_path_rewrites("/home/vscode");
        let input = r#"["/home/node/.claude/a", "/Users/alice/.claude/b"]"#;
        let result = rewrite_paths(input, &rewrites).unwrap();
        assert_eq!(
            result,
            r#"["/home/vscode/.claude/a", "/home/vscode/.claude/b"]"#
        );
    }

    #[test]
    fn rewrite_noop_when_already_correct() {
        let rewrites = build_path_rewrites("/home/vscode");
        let input = r#"{"path": "/home/vscode/.claude/plugins"}"#;
        let result = rewrite_paths(input, &rewrites);
        // Still matches and replaces, but result is identical
        assert!(result.is_none() || result.as_deref() == Some(input));
    }

    #[test]
    fn rewrite_does_not_match_non_claude() {
        let rewrites = build_path_rewrites("/home/vscode");
        let input = r#"{"path": "/home/node/.config/gh/hosts.yml"}"#;
        let result = rewrite_paths(input, &rewrites);
        assert!(result.is_none());
    }

    // --- should_rewrite ---

    #[test]
    fn rewrite_json_extension() {
        assert!(should_rewrite("plugins/installed_plugins.json"));
    }

    #[test]
    fn rewrite_sh_extension() {
        assert!(should_rewrite("hooks/precompact.sh"));
    }

    #[test]
    fn skip_binary_extension() {
        assert!(!should_rewrite("cache/some_binary.bin"));
    }

    #[test]
    fn skip_no_extension() {
        assert!(!should_rewrite("somefile"));
    }

    // --- is_user_excluded ---

    #[test]
    fn user_excluded_matches() {
        assert!(is_user_excluded("plans", &["plans/**".to_string()]));
    }

    #[test]
    fn user_excluded_no_match() {
        assert!(!is_user_excluded("plugins", &["plans/**".to_string()]));
    }

    // --- claude_config_exists_command ---

    #[test]
    fn config_exists_command() {
        let cmd = claude_config_exists_command("/home/vscode/.claude");
        assert_eq!(cmd, vec!["test", "-d", "/home/vscode/.claude"]);
    }

    // --- maybe_rewrite ---

    #[test]
    fn maybe_rewrite_text_file() {
        let rewrites = build_path_rewrites("/home/vscode");
        let content = b"/home/node/.claude/plugins";
        let result = maybe_rewrite(content, "test.json", &rewrites);
        assert_eq!(result, b"/home/vscode/.claude/plugins");
    }

    #[test]
    fn maybe_rewrite_binary_file_unchanged() {
        let rewrites = build_path_rewrites("/home/vscode");
        let content = b"/home/node/.claude/plugins";
        let result = maybe_rewrite(content, "test.bin", &rewrites);
        assert_eq!(result, content);
    }

    #[test]
    fn maybe_rewrite_invalid_utf8() {
        let rewrites = build_path_rewrites("/home/vscode");
        let content: &[u8] = &[0xFF, 0xFE, 0x00, 0x01];
        let result = maybe_rewrite(content, "test.json", &rewrites);
        assert_eq!(result, content);
    }

    // --- is_sensitive_file ---

    #[test]
    fn sensitive_hidden_json() {
        assert!(is_sensitive_file(".credentials.json"));
    }

    #[test]
    fn sensitive_not_hidden() {
        assert!(!is_sensitive_file("settings.json"));
    }

    #[test]
    fn sensitive_not_json() {
        assert!(!is_sensitive_file(".bashrc"));
    }

    // --- heuristic_file_mode ---

    #[test]
    fn heuristic_sh_extension() {
        assert_eq!(heuristic_file_mode(b"echo hi", "hook.sh"), 0o755);
    }

    #[test]
    fn heuristic_bash_extension() {
        assert_eq!(heuristic_file_mode(b"echo hi", "hook.bash"), 0o755);
    }

    #[test]
    fn heuristic_py_extension() {
        assert_eq!(heuristic_file_mode(b"print('hi')", "script.py"), 0o755);
    }

    #[test]
    fn heuristic_json_not_executable() {
        assert_eq!(heuristic_file_mode(b"{}", "config.json"), 0o644);
    }

    #[test]
    fn heuristic_shebang_no_extension() {
        assert_eq!(
            heuristic_file_mode(b"#!/bin/bash\necho hi", "myscript"),
            0o755
        );
    }

    #[test]
    fn heuristic_no_shebang_no_extension() {
        assert_eq!(heuristic_file_mode(b"just text", "readme"), 0o644);
    }

    #[test]
    fn heuristic_shebang_with_unknown_extension() {
        // File has .txt extension — shebang check skipped for files with extensions.
        assert_eq!(
            heuristic_file_mode(b"#!/bin/bash\necho hi", "notes.txt"),
            0o644
        );
    }

    // --- resolve_file_mode ---

    #[cfg(unix)]
    #[test]
    fn resolve_mode_reads_host_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test_script");
        std::fs::write(&file_path, b"#!/bin/sh\necho hi").unwrap();
        std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(0o750)).unwrap();

        let content = std::fs::read(&file_path).unwrap();
        let mode = resolve_file_mode(&file_path, &content, "test_script");
        assert_eq!(mode, 0o750);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_mode_sensitive_overrides_host() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join(".credentials.json");
        std::fs::write(&file_path, b"{}").unwrap();
        std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let content = std::fs::read(&file_path).unwrap();
        let mode = resolve_file_mode(&file_path, &content, ".credentials.json");
        assert_eq!(
            mode, 0o600,
            "Sensitive files must be capped at 0o600 regardless of host mode"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_mode_preserves_non_standard_triple() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("deploy.sh");
        std::fs::write(&file_path, b"#!/bin/sh").unwrap();
        std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(0o700)).unwrap();

        let content = std::fs::read(&file_path).unwrap();
        let mode = resolve_file_mode(&file_path, &content, "deploy.sh");
        assert_eq!(
            mode, 0o700,
            "Full mode triple should be preserved, not normalized to 0o755"
        );
    }

    #[test]
    fn resolve_mode_falls_back_to_heuristic() {
        let nonexistent = Path::new("/tmp/does_not_exist_cella_test_xyz");
        let mode = resolve_file_mode(nonexistent, b"#!/usr/bin/env python3", "myscript");
        assert_eq!(
            mode, 0o755,
            "Should fall back to shebang heuristic when metadata unavailable"
        );
    }
}
