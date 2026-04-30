//! Host global gitignore resolution and container forwarding.

use std::path::{Path, PathBuf};

use crate::claude_code::container_home;

const HOST_MARKER: &str = "# --- forwarded from host ---";
const HOST_UPLOAD_PATH: &str = "/tmp/.cella/host-gitignore";

/// Resolve the host's global gitignore file path.
///
/// Checks `core.excludesFile` via git first, then falls back to the
/// XDG default location.
pub fn resolve_host_gitignore_path() -> Option<PathBuf> {
    if let Some(path) = resolve_from_git_config() {
        return Some(path);
    }

    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    let home = std::env::var("HOME").ok().unwrap_or_default();
    resolve_gitignore_from_xdg(xdg.as_deref(), &home)
}

fn resolve_from_git_config() -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["config", "--global", "--path", "core.excludesFile"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let path_str = String::from_utf8_lossy(&output.stdout);
    let path = PathBuf::from(path_str.trim());
    if path.is_file() { Some(path) } else { None }
}

fn resolve_gitignore_from_xdg(xdg_config_home: Option<&str>, home: &str) -> Option<PathBuf> {
    let config_base = xdg_config_home
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home).join(".config"));

    let path = config_base.join("git/ignore");
    if path.is_file() { Some(path) } else { None }
}

fn read_gitignore_file(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(content) if !content.is_empty() => Some(content),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("Failed to read global gitignore at {}: {e}", path.display());
            None
        }
    }
}

pub fn read_host_gitignore() -> Option<String> {
    let path = resolve_host_gitignore_path()?;
    read_gitignore_file(&path)
}

pub fn cella_ignore_path(remote_user: &str) -> String {
    format!("{}/.config/git/cella-ignore", container_home(remote_user))
}

pub fn host_upload_path() -> &'static str {
    HOST_UPLOAD_PATH
}

fn build_merge_script(
    git_config_dir: &str,
    container_ignore: &str,
    cella_ignore: &str,
    upload_path: &str,
) -> String {
    format!(
        r#"mkdir -p {git_config_dir}
{{
if [ -f {container_ignore} ]; then
sed '/{marker}/,$d' {container_ignore}
fi
printf '%s\n' '{marker}'
cat {upload_path}
}} > {cella_ignore}"#,
        git_config_dir = git_config_dir,
        container_ignore = container_ignore,
        cella_ignore = cella_ignore,
        marker = HOST_MARKER,
        upload_path = upload_path,
    )
}

pub fn build_merge_commands(remote_user: &str, upload_path: &str) -> Vec<Vec<String>> {
    let home = container_home(remote_user);
    let git_config_dir = format!("{home}/.config/git");
    let container_ignore = format!("{git_config_dir}/ignore");
    let cella_ignore = format!("{git_config_dir}/cella-ignore");

    let script = build_merge_script(
        &git_config_dir,
        &container_ignore,
        &cella_ignore,
        upload_path,
    );

    vec![vec!["sh".to_string(), "-c".to_string(), script]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn resolve_xdg_default() {
        let tmp = TempDir::new().unwrap();
        let git_dir = tmp.path().join("git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("ignore"), "*.log\n").unwrap();

        let path = resolve_gitignore_from_xdg(Some(tmp.path().to_str().unwrap()), "/nonexistent");
        assert_eq!(path.unwrap(), git_dir.join("ignore"));
    }

    #[test]
    fn resolve_home_default() {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join(".config/git");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(config_dir.join("ignore"), "*.log\n").unwrap();

        let path = resolve_gitignore_from_xdg(None, tmp.path().to_str().unwrap());
        assert_eq!(path.unwrap(), config_dir.join("ignore"));
    }

    #[test]
    fn resolve_missing_returns_none() {
        let path = resolve_gitignore_from_xdg(None, "/nonexistent/path");
        assert!(path.is_none());
    }

    #[test]
    fn read_returns_content() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("ignore");
        fs::write(&file, "*.log\n.DS_Store\n").unwrap();

        let content = read_gitignore_file(&file);
        assert_eq!(content.unwrap(), "*.log\n.DS_Store\n");
    }

    #[test]
    fn read_empty_file_returns_none() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("ignore");
        fs::write(&file, "").unwrap();

        let content = read_gitignore_file(&file);
        assert!(content.is_none());
    }

    #[test]
    fn cella_ignore_path_root() {
        assert_eq!(cella_ignore_path("root"), "/root/.config/git/cella-ignore");
    }

    #[test]
    fn cella_ignore_path_regular_user() {
        assert_eq!(
            cella_ignore_path("vscode"),
            "/home/vscode/.config/git/cella-ignore"
        );
    }

    #[test]
    fn merge_commands_produces_single_command() {
        let cmds = build_merge_commands("vscode", "/tmp/.cella/host-gitignore");
        assert_eq!(cmds.len(), 1);
        let script = &cmds[0][2]; // sh -c <script>
        assert!(script.contains("/home/vscode/.config/git"));
        assert!(script.contains("# --- forwarded from host ---"));
    }

    #[test]
    fn merge_commands_root_user() {
        let cmds = build_merge_commands("root", "/tmp/.cella/host-gitignore");
        assert_eq!(cmds.len(), 1);
        let script = &cmds[0][2];
        assert!(script.contains("/root/.config/git"));
    }

    #[test]
    fn merge_script_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let git_dir = tmp.path().join(".config/git");
        fs::create_dir_all(&git_dir).unwrap();

        let container_ignore = git_dir.join("ignore");
        fs::write(&container_ignore, "local-pattern\n").unwrap();

        let upload = tmp.path().join("host-gitignore");
        fs::write(&upload, "*.log\n.DS_Store\n").unwrap();

        let cella_ignore = git_dir.join("cella-ignore");

        let script = build_merge_script(
            git_dir.to_str().unwrap(),
            container_ignore.to_str().unwrap(),
            cella_ignore.to_str().unwrap(),
            upload.to_str().unwrap(),
        );

        // Run twice
        for _ in 0..2 {
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(&script)
                .status()
                .unwrap();
            assert!(status.success());
        }

        let content = fs::read_to_string(&cella_ignore).unwrap();
        assert_eq!(
            content,
            "local-pattern\n# --- forwarded from host ---\n*.log\n.DS_Store\n"
        );
    }

    #[test]
    fn merge_script_no_container_ignore() {
        let tmp = TempDir::new().unwrap();
        let git_dir = tmp.path().join(".config/git");

        let container_ignore = git_dir.join("ignore");
        let cella_ignore = git_dir.join("cella-ignore");

        let upload = tmp.path().join("host-gitignore");
        fs::write(&upload, "*.log\n").unwrap();

        let script = build_merge_script(
            git_dir.to_str().unwrap(),
            container_ignore.to_str().unwrap(),
            cella_ignore.to_str().unwrap(),
            upload.to_str().unwrap(),
        );

        // Run twice — must be idempotent even without container ignore
        for _ in 0..2 {
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(&script)
                .status()
                .unwrap();
            assert!(status.success());
        }

        let content = fs::read_to_string(&cella_ignore).unwrap();
        assert_eq!(content, "# --- forwarded from host ---\n*.log\n");
    }
}
