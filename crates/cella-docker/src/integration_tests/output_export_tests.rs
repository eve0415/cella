//! Integration test: `build_image` with a buildx `--output type=local,dest=…`
//! export writes the built filesystem to the destination directory.
//!
//! This is the backend primitive behind `cella build --output <spec>`: when an
//! output spec is set, `build_image` runs `docker buildx build --output <spec>`
//! (replacing the default `--load`), so the result is exported to the local
//! filesystem rather than loaded into the docker image store. No registry is
//! needed. The test builds a trivial Dockerfile that writes a sentinel file and
//! asserts that file lands under `dest=`.

use std::collections::HashMap;
use std::path::PathBuf;

use cella_backend::BuildOptions;

use crate::client::DockerClient;

/// Whether `docker buildx` is available. `--output` is buildx-only, so the test
/// skips (rather than fails) when buildx is absent — `build_image` would
/// correctly return an error in that case, which is its own unit-tested path.
fn has_buildx() -> bool {
    std::process::Command::new("docker")
        .args(["buildx", "version"])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A unique scratch directory under the system temp dir, removed first so a
/// stale run can't mask a failure. Mirrors the `/tmp`-path style of the other
/// integration tests (no `tempfile` dev-dependency in this crate).
fn scratch_dir(suffix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cella-it-output-export-{}-{suffix}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// `build_image` with `--output type=local,dest=<dir>` exports the built image's
/// filesystem to `<dir>`. The Dockerfile creates `/cella-export-sentinel`, so
/// after the build that file must exist under the destination directory.
#[cella_testing::runtime_test(docker)]
async fn output_type_local_exports_to_dest() {
    let Ok(client) = DockerClient::connect() else {
        return;
    };
    // `--output` needs buildx; skip cleanly when it isn't installed.
    if !has_buildx() {
        return;
    }

    let context = scratch_dir("ctx");
    let dest = scratch_dir("dest");
    if std::fs::create_dir_all(&context).is_err() || std::fs::create_dir_all(&dest).is_err() {
        return;
    }

    // Trivial, registry-free build: scratch base + a single sentinel file.
    let dockerfile = "FROM busybox:latest\nRUN echo cella > /cella-export-sentinel\n";
    if std::fs::write(context.join("Dockerfile"), dockerfile).is_err() {
        return;
    }
    // Pre-pull the base so the build itself needs no registry round-trip beyond
    // the (cached) base; if the pull fails (offline), skip rather than fail.
    if client.pull_image("busybox:latest").await.is_err() {
        return;
    }

    let opts = BuildOptions {
        image_name: format!("cella-it-output-export-{}:test", std::process::id()),
        context_path: context.clone(),
        dockerfile: "Dockerfile".to_string(),
        args: HashMap::new(),
        target: None,
        cache_from: Vec::new(),
        cache_to: None,
        options: Vec::new(),
        secrets: Vec::new(),
        use_buildkit: true,
        docker_path: None,
        platform: None,
        output: Some(format!("type=local,dest={}", dest.display())),
    };

    let result = client.build_image(&opts, |_| {}).await;

    // Sentinel exported to the destination → the `--output` export landed.
    let sentinel = dest.join("cella-export-sentinel");
    let exported = sentinel.exists();

    // Best-effort cleanup of both scratch dirs.
    let _ = std::fs::remove_dir_all(&context);
    let _ = std::fs::remove_dir_all(&dest);

    result.expect("build_image with --output type=local should succeed");
    assert!(
        exported,
        "expected exported sentinel at {} after --output type=local export",
        sentinel.display()
    );
}
