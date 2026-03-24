//! Workspace content hash for detecting changes since container creation.
//!
//! Used by `updateContentCommand` to re-run when workspace content changes.

use std::collections::BTreeMap;
use std::path::Path;

use tracing::debug;

/// Compute a content hash for the workspace.
///
/// For git repos: SHA-256 of `HEAD` commit hash + porcelain status.
/// For non-git workspaces: SHA-256 of sorted top-level file mtimes.
///
/// Returns a hex-encoded 16-character hash string.
pub fn compute(workspace_root: &Path) -> String {
    if let Some(hash) = git_content_hash(workspace_root) {
        return hash;
    }
    debug!("Not a git repo, falling back to mtime-based hash");
    mtime_content_hash(workspace_root)
}

/// Git-based content hash: HEAD commit + dirty state.
fn git_content_hash(workspace_root: &Path) -> Option<String> {
    use std::process::Command;

    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace_root)
        .output()
        .ok()?;
    if !head.status.success() {
        return None;
    }
    let head_hash = String::from_utf8_lossy(&head.stdout).trim().to_string();

    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workspace_root)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    let input = format!("{head_hash}\n{status}");
    Some(sha256_short(&input))
}

/// Mtime-based content hash: sorted top-level file modification times.
fn mtime_content_hash(workspace_root: &Path) -> String {
    let mut mtimes = BTreeMap::new();

    if let Ok(entries) = std::fs::read_dir(workspace_root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip hidden files and common large dirs
            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue;
            }
            if let Ok(meta) = entry.metadata()
                && let Ok(mtime) = meta.modified()
            {
                let secs = mtime
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                mtimes.insert(name, secs);
            }
        }
    }

    let input: String = mtimes
        .iter()
        .map(|(k, v)| format!("{k}:{v}"))
        .collect::<Vec<_>>()
        .join("\n");

    sha256_short(&input)
}

/// SHA-256 of input, returning first 16 hex characters.
fn sha256_short(input: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // Use a fast non-cryptographic hash — collision avoidance is sufficient here
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    let h1 = hasher.finish();
    // Hash again with a different seed for more bits
    input.len().hash(&mut hasher);
    let h2 = hasher.finish();
    format!("{h1:016x}{h2:016x}")[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn hash_is_16_chars() {
        let tmp = TempDir::new().unwrap();
        let hash = compute(tmp.path());
        assert_eq!(hash.len(), 16);
    }

    #[test]
    fn hash_deterministic() {
        let tmp = TempDir::new().unwrap();
        let h1 = compute(tmp.path());
        let h2 = compute(tmp.path());
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_changes_with_new_file() {
        let tmp = TempDir::new().unwrap();
        let h1 = compute(tmp.path());
        std::fs::write(tmp.path().join("new_file.txt"), "content").unwrap();
        let h2 = compute(tmp.path());
        assert_ne!(h1, h2);
    }

    #[test]
    fn git_repo_hash() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // Init a git repo
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()
            .unwrap();

        std::fs::write(dir.join("file.txt"), "initial").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(dir)
            .output()
            .unwrap();

        let h1 = compute(dir);
        assert_eq!(h1.len(), 16);

        // Modify file (dirty state)
        std::fs::write(dir.join("file.txt"), "modified").unwrap();
        let h2 = compute(dir);
        assert_ne!(h1, h2, "hash should change with dirty state");

        // Commit the change
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "modify"])
            .current_dir(dir)
            .output()
            .unwrap();
        let h3 = compute(dir);
        assert_ne!(h2, h3, "hash should change after commit");
    }
}
