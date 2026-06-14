//! Integration test: `build_image` with `--label key=value` bakes that label
//! into the built image.
//!
//! This is the backend primitive behind `cella build --label`: when labels are
//! set, `build_image` runs `docker [buildx] build --label key=value`, so the
//! label is baked into the resulting image. The test builds a trivial Dockerfile
//! with `--label cella.test=1`, then inspects the built image and asserts the
//! label is present. `--label` works on both the classic and buildx builders, so
//! (unlike the `--output` export test) this needs no buildx and no registry.
//!
//! `inspect_image_details` only surfaces the `devcontainer.metadata` label, so
//! this reads the raw label map via `client.inner().inspect_image`.

use std::collections::HashMap;

use bollard::query_parameters::RemoveImageOptions;

use cella_backend::BuildOptions;

use crate::client::DockerClient;

/// A unique scratch directory under the system temp dir, removed first so a
/// stale run can't mask a failure (mirrors `output_export_tests`).
fn scratch_dir(suffix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cella-it-label-build-{}-{suffix}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// `build_image` with `--label cella.test=1` bakes that label into the built
/// image. After the build, inspecting the image must report the label — this is
/// the validation that matters: the label actually lands on the image, not just
/// that the arg was emitted.
#[cella_testing::runtime_test(docker)]
async fn label_is_baked_into_built_image() {
    let Ok(client) = DockerClient::connect() else {
        return;
    };

    let context = scratch_dir("ctx");
    // Setup failures are real bugs (not an unavailable runtime) — fail loudly.
    std::fs::create_dir_all(&context).expect("create build context dir");

    // Trivial, registry-free build once the base is cached.
    let dockerfile = "FROM busybox:latest\n";
    std::fs::write(context.join("Dockerfile"), dockerfile).expect("write test Dockerfile");
    // Pre-pull the base so the build needs no registry round-trip beyond the
    // (cached) base; if the pull fails (offline), skip rather than fail.
    if client.pull_image("busybox:latest").await.is_err() {
        return;
    }

    let image_name = format!("cella-it-label-build-{}:test", std::process::id());
    let opts = BuildOptions {
        image_name: image_name.clone(),
        context_path: context.clone(),
        dockerfile: "Dockerfile".to_string(),
        labels: vec!["cella.test=1".to_string()],
        ..Default::default()
    };

    let build_result = client.build_image(&opts, |_| {}).await;

    // Read the raw label map (inspect_image_details only exposes the metadata
    // label, so go through bollard directly).
    let label_value = match client.inner().inspect_image(&image_name).await {
        Ok(details) => details
            .config
            .as_ref()
            .and_then(|c| c.labels.as_ref())
            .and_then(|labels: &HashMap<String, String>| labels.get("cella.test"))
            .cloned(),
        Err(_) => None,
    };

    // Best-effort cleanup of the scratch dir and the image we built.
    let _ = std::fs::remove_dir_all(&context);
    let cleanup_opts = RemoveImageOptions {
        force: true,
        ..Default::default()
    };
    let _ = client
        .inner()
        .remove_image(&image_name, Some(cleanup_opts), None)
        .await;

    build_result.expect("build_image with --label should succeed");
    assert_eq!(
        label_value.as_deref(),
        Some("1"),
        "expected label cella.test=1 on the built image {image_name}, got {label_value:?}"
    );
}
