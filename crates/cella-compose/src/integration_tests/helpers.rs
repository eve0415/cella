//! Test helpers for compose integration tests.
//!
//! Provides fixture copying, unique project naming, and cleanup utilities
//! for tests that exercise the full compose lifecycle.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use tempfile::TempDir;

use crate::cli::ComposeCommand;

/// Monotonic counter for unique project names across test threads.
static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Test context that owns a temp directory, fixture copy, and project name.
///
/// On drop, attempts a best-effort `docker compose down` to avoid leaked
/// containers.  Tests should call [`cleanup`](Self::cleanup) explicitly for
/// proper error reporting; the `Drop` impl is a safety net only.
pub struct ComposeTestContext {
    /// Unique compose project name for this test.
    pub project_name: String,
    /// Temp directory containing the copied fixture files.
    pub fixture_dir: PathBuf,
    /// Temp directory handle (dropped when context is dropped).
    _temp_dir: TempDir,
}

impl ComposeTestContext {
    /// Copy a named fixture into a fresh temp directory and generate a unique
    /// project name.
    ///
    /// `fixture_name` should match a directory under `test_fixtures/`
    /// (e.g., `"plain-compose"`).
    pub fn new(fixture_name: &str) -> Self {
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let project_name = format!("cella-test-{fixture_name}-{pid}-{counter}");

        let temp_dir = TempDir::new().expect("failed to create temp directory");
        let fixture_src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_fixtures")
            .join(fixture_name);
        let fixture_dir = temp_dir.path().to_path_buf();

        copy_dir_recursive(&fixture_src, &fixture_dir)
            .expect("failed to copy fixture into temp dir");

        Self {
            project_name,
            fixture_dir,
            _temp_dir: temp_dir,
        }
    }

    /// Run `docker compose down --volumes --remove-orphans` for thorough cleanup.
    pub async fn cleanup(&self) {
        let cmd = ComposeCommand::from_project_name(&self.project_name);
        let _ = cmd.down_and_clean().await;
    }
}

impl Drop for ComposeTestContext {
    fn drop(&mut self) {
        // Best-effort synchronous cleanup via a spawned blocking task.
        // Tests should call cleanup() explicitly; this is a safety net.
        let name = self.project_name.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build cleanup runtime");
            rt.block_on(async {
                let cmd = ComposeCommand::from_project_name(&name);
                let _ = cmd.down_and_clean().await;
            });
        });
    }
}

/// Recursively copy a directory tree into an existing destination.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Create a minimal devcontainer feature directory for testing.
///
/// Writes `devcontainer-feature.json` and `install.sh` into `dir/id`.
pub fn create_test_feature(dir: &Path, id: &str) {
    let feature_dir = dir.join(id);
    std::fs::create_dir_all(&feature_dir).expect("failed to create feature dir");

    let meta = serde_json::json!({
        "id": id,
        "version": "1.0.0",
        "name": id,
        "installsAfter": []
    });
    std::fs::write(
        feature_dir.join("devcontainer-feature.json"),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .expect("failed to write feature metadata");

    std::fs::write(
        feature_dir.join("install.sh"),
        "#!/bin/sh\necho \"Installing test feature\"\n",
    )
    .expect("failed to write install.sh");
}

/// Load a devcontainer.json fixture as a `serde_json::Value`.
pub fn load_fixture_config(dir: &Path) -> serde_json::Value {
    let path = dir.join("devcontainer.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture config at {}: {e}", path.display()));
    serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("failed to parse fixture config at {}: {e}", path.display()))
}
