//! The default single-container workspace layout: which host folder is
//! bind-mounted and where the workspace lands inside the container.
//!
//! Shared by `up`, `read-configuration` and `run-user-commands`, which must
//! agree on the default `workspaceFolder` (and therefore on the default
//! `${containerWorkspaceFolder}`) before the devcontainer config is resolved.

use std::path::{Path, PathBuf};

/// The default workspace layout for a host workspace folder.
pub struct DefaultWorkspace {
    /// The host folder bind-mounted into the container: the enclosing git
    /// root when `--mount-workspace-git-root` is on, else the workspace itself.
    pub host_mount_folder: PathBuf,
    /// The in-container workspace folder used when `workspaceFolder` is unset.
    pub container_workspace_folder: String,
}

impl DefaultWorkspace {
    /// Detect the layout for `workspace_root`.
    pub fn detect(workspace_root: &Path, mount_workspace_git_root: bool) -> Self {
        let host_mount_folder =
            cella_git::find_git_root_folder(workspace_root, mount_workspace_git_root);
        let container_workspace_folder =
            compute_default_workspace_folder(workspace_root, &host_mount_folder);
        Self {
            host_mount_folder,
            container_workspace_folder,
        }
    }
}

/// The default in-container workspace folder: `/workspaces/<mount basename>`,
/// extended by the workspace's path relative to `host_mount_folder` when the
/// workspace is a subdirectory of the mounted folder.
pub fn compute_default_workspace_folder(workspace_root: &Path, host_mount_folder: &Path) -> String {
    let mount_basename = host_mount_folder.file_name().map_or_else(
        || "workspace".to_string(),
        |n| n.to_string_lossy().to_string(),
    );
    let container_mount_folder = format!("/workspaces/{mount_basename}");
    if host_mount_folder == workspace_root {
        return container_mount_folder;
    }
    if let Ok(rel) = workspace_root.strip_prefix(host_mount_folder) {
        let rel_posix = rel.to_string_lossy().replace('\\', "/");
        if !rel_posix.is_empty() {
            return format!("{container_mount_folder}/{rel_posix}");
        }
    }
    container_mount_folder
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_at_mount_root_maps_to_mount_folder() {
        assert_eq!(
            compute_default_workspace_folder(Path::new("/src/repo"), Path::new("/src/repo")),
            "/workspaces/repo"
        );
    }

    #[test]
    fn workspace_below_mount_root_keeps_relative_path() {
        assert_eq!(
            compute_default_workspace_folder(
                Path::new("/src/repo/packages/app"),
                Path::new("/src/repo")
            ),
            "/workspaces/repo/packages/app"
        );
    }

    #[test]
    fn detect_without_git_root_mounts_the_workspace_itself() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        let layout = DefaultWorkspace::detect(&app, false);
        assert_eq!(layout.host_mount_folder, app);
        assert_eq!(layout.container_workspace_folder, "/workspaces/app");
    }

    /// A workspace in a git subdirectory resolves `${containerWorkspaceFolder}`
    /// to where the git-root mount actually puts it.
    #[test]
    fn git_subdirectory_workspace_resolves_container_workspace_folder() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().canonicalize().unwrap().join("repo");
        let app = repo.join("packages/app");
        std::fs::create_dir_all(app.join(".devcontainer")).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(init.success());
        std::fs::write(
            app.join(".devcontainer/devcontainer.json"),
            concat!(
                r#"{"image": "ubuntu", "mounts": ["source=x,target=${containerWorkspaceFolder"#,
                r#"}/x,type=volume"]}"#
            ),
        )
        .unwrap();

        let layout = DefaultWorkspace::detect(&app, true);
        assert_eq!(
            layout.container_workspace_folder,
            "/workspaces/repo/packages/app"
        );
        let options = cella_config::devcontainer::resolve::ResolveOptions {
            default_container_workspace_folder: Some(layout.container_workspace_folder),
            ..cella_config::devcontainer::resolve::ResolveOptions::default()
        };
        let resolved =
            cella_config::devcontainer::resolve::config_with_override(&app, None, None, &options)
                .unwrap();
        assert_eq!(
            resolved.config["mounts"][0],
            "source=x,target=/workspaces/repo/packages/app/x,type=volume"
        );
    }
}
