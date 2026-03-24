//! Multi-file change detection hashing for compose configs.

use std::path::Path;

use sha2::{Digest, Sha256};

/// Compute a SHA256 hash over the devcontainer.json config and all compose files.
///
/// The hash includes:
/// - The canonical JSON of the devcontainer config
/// - Each compose file's absolute path and content (sorted by path for determinism)
///
/// Returns a hex-encoded SHA256 string.
pub fn compute_compose_hash(
    config: &serde_json::Value,
    compose_files: &[impl AsRef<Path>],
) -> String {
    let mut hasher = Sha256::new();

    // Hash the devcontainer.json config (canonical JSON)
    let canonical = serde_json::to_string(config).unwrap_or_default();
    hasher.update(canonical.as_bytes());

    // Hash each compose file's content (sorted by path for determinism)
    let mut sorted_files: Vec<&Path> = compose_files.iter().map(AsRef::as_ref).collect();
    sorted_files.sort();
    for path in &sorted_files {
        if let Ok(content) = std::fs::read(path) {
            hasher.update(path.to_string_lossy().as_bytes());
            hasher.update(&content);
        }
    }

    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn write_file(dir: &tempfile::TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_file(&dir, "compose.yml", "services:\n  app:\n    image: node\n");
        let config = json!({"service": "app"});
        let h1 = compute_compose_hash(&config, &[&p]);
        let h2 = compute_compose_hash(&config, &[&p]);
        assert_eq!(h1, h2);
    }

    #[test]
    fn changes_with_compose_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = json!({"service": "app"});

        let p = write_file(&dir, "compose.yml", "services:\n  app:\n    image: node\n");
        let h1 = compute_compose_hash(&config, &[&p]);

        // Modify the compose file
        std::fs::write(&p, "services:\n  app:\n    image: node:20\n").unwrap();
        let h2 = compute_compose_hash(&config, &[&p]);

        assert_ne!(h1, h2);
    }

    #[test]
    fn changes_with_config() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_file(&dir, "compose.yml", "services:\n  app:\n    image: node\n");

        let h1 = compute_compose_hash(&json!({"service": "app"}), &[&p]);
        let h2 = compute_compose_hash(&json!({"service": "web"}), &[&p]);

        assert_ne!(h1, h2);
    }

    #[test]
    fn order_independent() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = write_file(&dir, "a.yml", "services:\n  app:\n    image: node\n");
        let p2 = write_file(&dir, "b.yml", "services:\n  db:\n    image: postgres\n");
        let config = json!({"service": "app"});

        let h1 = compute_compose_hash(&config, &[&p1, &p2]);
        let h2 = compute_compose_hash(&config, &[&p2, &p1]);

        assert_eq!(h1, h2);
    }
}
