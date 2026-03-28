//! Devcontainer-spec-compliant config file discovery.
//!
//! Searches from a workspace root (no parent traversal) in the order defined by
//! the dev container specification.

use std::path::{Path, PathBuf};

/// Errors that can occur during config discovery.
#[derive(Debug)]
pub enum Error {
    /// No devcontainer.json found in any standard location.
    NotFound,
    /// Multiple subfolder configs found — user must specify `--file`.
    Ambiguous(Vec<PathBuf>),
    /// Failed to read a directory during discovery.
    ReadDir {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(
                f,
                "no devcontainer.json found in .devcontainer/ or as .devcontainer.json"
            ),
            Self::Ambiguous(paths) => {
                write!(
                    f,
                    "multiple devcontainer configs found, use --file to specify one:"
                )?;
                for p in paths {
                    write!(f, "\n  {}", p.display())?;
                }
                Ok(())
            }
            Self::ReadDir { path, source } => {
                write!(f, "failed to read directory {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ReadDir { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Discover a devcontainer.json file from the given workspace root.
///
/// Search order (first match wins):
/// 1. `{root}/.devcontainer/devcontainer.json`
/// 2. `{root}/.devcontainer.json`
/// 3. `{root}/.devcontainer/<subfolder>/devcontainer.json` (one level deep)
///
/// If step 3 finds multiple configs, returns [`Error::Ambiguous`].
///
/// # Errors
///
/// Returns [`Error`] if no config is found, multiple ambiguous configs
/// exist, or a directory cannot be read.
pub fn config(workspace_root: &Path) -> Result<PathBuf, Error> {
    // 1. .devcontainer/devcontainer.json
    let primary = workspace_root
        .join(".devcontainer")
        .join("devcontainer.json");
    if primary.is_file() {
        return Ok(primary);
    }

    // 2. .devcontainer.json (root level)
    let root_level = workspace_root.join(".devcontainer.json");
    if root_level.is_file() {
        return Ok(root_level);
    }

    // 3. .devcontainer/<subfolder>/devcontainer.json
    let devcontainer_dir = workspace_root.join(".devcontainer");
    if devcontainer_dir.is_dir() {
        let entries = std::fs::read_dir(&devcontainer_dir).map_err(|source| Error::ReadDir {
            path: devcontainer_dir.clone(),
            source,
        })?;

        let mut found = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| Error::ReadDir {
                path: devcontainer_dir.clone(),
                source,
            })?;
            if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                let candidate = entry.path().join("devcontainer.json");
                if candidate.is_file() {
                    found.push(candidate);
                }
            }
        }

        match found.len() {
            0 => {}
            1 => return Ok(found.swap_remove(0)),
            _ => {
                found.sort();
                return Err(Error::Ambiguous(found));
            }
        }
    }

    Err(Error::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_file(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, "{}").unwrap();
    }

    #[test]
    fn primary_path_wins() {
        let tmp = TempDir::new().unwrap();
        let primary = tmp.path().join(".devcontainer").join("devcontainer.json");
        create_file(&primary);

        // Also create root-level to prove priority
        create_file(&tmp.path().join(".devcontainer.json"));

        let result = config(tmp.path()).unwrap();
        assert_eq!(result, primary);
    }

    #[test]
    fn root_level_fallback() {
        let tmp = TempDir::new().unwrap();
        let root_level = tmp.path().join(".devcontainer.json");
        create_file(&root_level);

        let result = config(tmp.path()).unwrap();
        assert_eq!(result, root_level);
    }

    #[test]
    fn subfolder_single() {
        let tmp = TempDir::new().unwrap();
        let subfolder = tmp
            .path()
            .join(".devcontainer")
            .join("rust")
            .join("devcontainer.json");
        create_file(&subfolder);

        let result = config(tmp.path()).unwrap();
        assert_eq!(result, subfolder);
    }

    #[test]
    fn subfolder_ambiguous() {
        let tmp = TempDir::new().unwrap();
        create_file(
            &tmp.path()
                .join(".devcontainer")
                .join("rust")
                .join("devcontainer.json"),
        );
        create_file(
            &tmp.path()
                .join(".devcontainer")
                .join("python")
                .join("devcontainer.json"),
        );

        let err = config(tmp.path()).unwrap_err();
        assert!(
            matches!(err, Error::Ambiguous(ref paths) if paths.len() == 2),
            "expected Ambiguous with 2 paths, got: {err}"
        );
        // Verify the display message
        let msg = err.to_string();
        assert!(msg.contains("--file"));
    }

    #[test]
    fn not_found() {
        let tmp = TempDir::new().unwrap();
        let err = config(tmp.path()).unwrap_err();
        assert!(
            matches!(err, Error::NotFound),
            "expected NotFound, got: {err}"
        );
    }

    #[test]
    fn primary_takes_priority_over_subfolder() {
        let tmp = TempDir::new().unwrap();
        let primary = tmp.path().join(".devcontainer").join("devcontainer.json");
        create_file(&primary);
        create_file(
            &tmp.path()
                .join(".devcontainer")
                .join("rust")
                .join("devcontainer.json"),
        );

        let result = config(tmp.path()).unwrap();
        assert_eq!(result, primary);
    }

    #[test]
    fn empty_devcontainer_dir_is_not_found() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".devcontainer")).unwrap();

        let err = config(tmp.path()).unwrap_err();
        assert!(matches!(err, Error::NotFound));
    }

    // --- Spec compliance tests ---
    // Reference: https://containers.dev/implementors/spec/#devcontainerjson

    #[test]
    fn spec_primary_takes_priority_over_root_level() {
        let tmp = TempDir::new().unwrap();
        let primary = tmp.path().join(".devcontainer").join("devcontainer.json");
        create_file(&primary);
        create_file(&tmp.path().join(".devcontainer.json"));
        let result = config(tmp.path()).unwrap();
        assert_eq!(result, primary);
    }

    #[test]
    fn spec_config_path_override_skips_discovery() {
        let tmp = TempDir::new().unwrap();
        let custom_path = tmp.path().join("custom").join("my-devcontainer.json");
        create_file(&custom_path);
        let resolved =
            crate::resolve::config(tmp.path(), Some(&custom_path)).unwrap();
        assert_eq!(resolved.config_path, custom_path);
    }

    #[test]
    fn spec_no_parent_traversal() {
        let tmp = TempDir::new().unwrap();
        create_file(
            &tmp.path()
                .join(".devcontainer")
                .join("devcontainer.json"),
        );
        let subdir = tmp.path().join("subproject");
        std::fs::create_dir_all(&subdir).unwrap();
        let result = config(&subdir);
        assert!(result.is_err(), "should not traverse parent directories");
    }
}
