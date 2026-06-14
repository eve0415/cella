//! Smoke tests: full compose lifecycle with Docker.

use std::collections::BTreeMap;

use super::helpers::{ComposeTestContext, load_fixture_config};
use crate::cli::ComposeCommand;
use crate::override_file::{OverrideConfig, generate_override_yaml, write_override_file};
use crate::project::ComposeProject;

/// Build override config for a plain service (no features, no build override).
fn plain_override(service: &str) -> OverrideConfig {
    OverrideConfig {
        primary_service: service.to_string(),
        image_override: None,
        override_command: false,
        agent_volume_name: "cella-agent".to_string(),
        agent_volume_target: "/cella".to_string(),
        extra_env: Vec::new(),
        extra_labels: BTreeMap::new(),
        build_dockerfile: None,
        build_target: None,
        build_context: None,
        additional_contexts: BTreeMap::new(),
        build_secrets: Vec::new(),
        build_labels: Vec::new(),
        extra_volumes: Vec::new(),
        request_gpu: false,
        security: cella_config::config_map::MergedSecurityConfig::default(),
        feature_entrypoints: Vec::new(),
        user_entrypoint: Vec::new(),
        user_command: None,
        build_only: false,
    }
}

/// Plain compose: write override -> build -> up -> ps -> down.
#[cella_testing::runtime_test(docker, compose)]
async fn plain_compose_lifecycle() {
    let ctx = ComposeTestContext::new("plain-compose");
    let config = load_fixture_config(&ctx.fixture_dir);
    let config_path = ctx.fixture_dir.join("devcontainer.json");
    let project = ComposeProject::from_resolved(&config, &config_path, &ctx.fixture_dir)
        .unwrap()
        .with_project_name(ctx.project_name.clone());

    // Ensure the cella-agent volume exists (create if not)
    let _ = tokio::process::Command::new("docker")
        .args(["volume", "create", "cella-agent"])
        .output()
        .await;

    // Write override file
    let override_cfg = plain_override(&project.primary_service);
    let yaml = generate_override_yaml(&override_cfg);
    write_override_file(&project.override_file, &yaml).unwrap();

    let cmd = ComposeCommand::new(&project);

    // Build (pulls images)
    cmd.build(None, false).await.expect("compose build failed");

    // Up
    cmd.up(None, false).await.expect("compose up failed");

    // Check running
    let statuses = cmd.ps_json().await.expect("compose ps failed");
    assert!(
        !statuses.is_empty(),
        "at least one service should be running"
    );
    let app_status = statuses.iter().find(|s| s.service == "app");
    assert!(
        app_status.is_some(),
        "app service should appear in ps output"
    );
    assert_eq!(
        app_status.unwrap().state,
        "running",
        "app service should be running"
    );

    // Cleanup
    ctx.cleanup().await;
    crate::override_file::cleanup_override_file(&project.override_file);
}

/// Image-only service with features: write override with build config -> up -> ps -> down.
///
/// This test verifies the override file generation and compose lifecycle
/// for an image-only service that would normally get features injected.
/// It does not perform actual feature resolution (that requires cella-features),
/// but confirms the override structure is accepted by Docker Compose.
#[cella_testing::runtime_test(docker, compose)]
async fn image_only_features_lifecycle() {
    let ctx = ComposeTestContext::new("image-only-features");
    let config = load_fixture_config(&ctx.fixture_dir);
    let config_path = ctx.fixture_dir.join("devcontainer.json");
    let project = ComposeProject::from_resolved(&config, &config_path, &ctx.fixture_dir)
        .unwrap()
        .with_project_name(ctx.project_name.clone());

    // Ensure the cella-agent volume exists
    let _ = tokio::process::Command::new("docker")
        .args(["volume", "create", "cella-agent"])
        .output()
        .await;

    // For this test, write a simple override without full feature resolution.
    // This validates that the override file structure works with compose.
    let override_cfg = plain_override(&project.primary_service);
    let yaml = generate_override_yaml(&override_cfg);
    write_override_file(&project.override_file, &yaml).unwrap();

    let cmd = ComposeCommand::new(&project);

    // Up (pulls image directly, no build needed for image-only)
    cmd.up(None, false).await.expect("compose up failed");

    // Check running
    let statuses = cmd.ps_json().await.expect("compose ps failed");
    assert!(
        !statuses.is_empty(),
        "at least one service should be running"
    );
    let app_running = statuses
        .iter()
        .any(|s| s.service == "app" && s.state == "running");
    assert!(app_running, "app service should be running");

    // Down
    ctx.cleanup().await;
    crate::override_file::cleanup_override_file(&project.override_file);
}

/// Whether a Docker image exists locally (`docker image inspect`).
async fn docker_image_exists(image: &str) -> bool {
    tokio::process::Command::new("docker")
        .args(["image", "inspect", image])
        .output()
        .await
        .is_ok_and(|o| o.status.success())
}

/// Read a single label's value off a built image via
/// `docker image inspect --format '{{ index .Config.Labels "<key>" }}'`.
///
/// Returns the trimmed value, or `None` if the inspect fails or the label is
/// absent (Go templates print `<no value>` for a missing key).
async fn docker_image_label(image: &str, key: &str) -> Option<String> {
    let format = format!("{{{{ index .Config.Labels {key:?} }}}}");
    let output = tokio::process::Command::new("docker")
        .args(["image", "inspect", "--format", &format, image])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() || value == "<no value>" {
        None
    } else {
        Some(value)
    }
}

/// No-features image-only compose: `cella build` must report the service's real
/// image (e.g. `alpine:3.21`), not the `"(compose)"` sentinel.
///
/// No existence check here: `docker compose build` is a no-op for an image-only
/// service (it neither builds nor pulls), so the image's local presence is not a
/// guarantee of this path — the resolved name is what the fix is about.
#[cella_testing::runtime_test(docker, compose)]
async fn no_features_reports_image_only_name() {
    let ctx = ComposeTestContext::new("plain-compose");
    let config = load_fixture_config(&ctx.fixture_dir);
    let config_path = ctx.fixture_dir.join("devcontainer.json");
    let project = ComposeProject::from_resolved(&config, &config_path, &ctx.fixture_dir)
        .unwrap()
        .with_project_name(ctx.project_name.clone());

    // No override file is written on the no-features path, so resolve the image
    // through a command without it (mirrors `compose_build`).
    let cmd = ComposeCommand::without_override(&project);
    cmd.build(None, false).await.expect("compose build failed");

    let image_name = crate::build_features::resolve_primary_service_image(&cmd, &project)
        .await
        .expect("resolving primary service image failed");

    assert_ne!(image_name, "(compose)", "must not return the sentinel");
    assert_eq!(image_name, "alpine:3.21", "should be the service's image");

    ctx.cleanup().await;
}

/// No-features build-based compose: `cella build` must report the
/// `{project}-{service}` image Docker Compose produces, not `"(compose)"`,
/// and that image must exist locally after the build.
#[cella_testing::runtime_test(docker, compose)]
async fn no_features_reports_build_image_name() {
    let ctx = ComposeTestContext::new("build-no-features");
    let config = load_fixture_config(&ctx.fixture_dir);
    let config_path = ctx.fixture_dir.join("devcontainer.json");
    let project = ComposeProject::from_resolved(&config, &config_path, &ctx.fixture_dir)
        .unwrap()
        .with_project_name(ctx.project_name.clone());

    let cmd = ComposeCommand::without_override(&project);
    cmd.build(None, false).await.expect("compose build failed");

    let image_name = crate::build_features::resolve_primary_service_image(&cmd, &project)
        .await
        .expect("resolving primary service image failed");

    let expected = format!("{}-app", project.project_name);
    assert_ne!(image_name, "(compose)", "must not return the sentinel");
    assert_eq!(
        image_name, expected,
        "build-based service should resolve to the project-service image name"
    );
    assert!(
        docker_image_exists(&image_name).await,
        "resolved image '{image_name}' should exist after build"
    );

    ctx.cleanup().await;
}

/// `--label` on a no-features, build-based compose service (sub-case 2): a
/// labels-only override (only `build.labels`; dockerfile/context inherited from
/// the base compose via `-f` merge) must bake the label into the built image.
///
/// Drives the same `build_only` override shape `compose_build` writes via
/// `write_labels_only_override`, then builds with the override (`new`) and
/// inspects the resulting `{project}-app` image. Because the override is
/// `build_only` it carries no agent volume, so the build runs WITHOUT the
/// `cella-agent` volume pre-created — also regression-guarding that a
/// labels-only build never references an unprovisioned external volume.
#[cella_testing::runtime_test(docker, compose)]
async fn label_lands_on_no_features_build_image() {
    let ctx = ComposeTestContext::new("build-no-features");
    let config = load_fixture_config(&ctx.fixture_dir);
    let config_path = ctx.fixture_dir.join("devcontainer.json");
    let project = ComposeProject::from_resolved(&config, &config_path, &ctx.fixture_dir)
        .unwrap()
        .with_project_name(ctx.project_name.clone());

    // Labels-only override: no dockerfile/context (inherited from base compose),
    // and `build_only` — exactly what `write_labels_only_override` emits, so the
    // override carries no agent volume. The `cella-agent` volume is deliberately
    // NOT pre-created here: the build must succeed without it, proving a
    // build-only override never references an unprovisioned external volume.
    let mut override_cfg = plain_override(&project.primary_service);
    override_cfg.build_labels = vec!["cella.test=2".to_string()];
    override_cfg.build_only = true;
    let yaml = generate_override_yaml(&override_cfg);
    write_override_file(&project.override_file, &yaml).unwrap();

    let cmd = ComposeCommand::new(&project);
    cmd.build(None, false).await.expect("compose build failed");

    let image_name = format!("{}-app", project.project_name);
    let label = docker_image_label(&image_name, "cella.test").await;

    ctx.cleanup().await;
    crate::override_file::cleanup_override_file(&project.override_file);

    assert_eq!(
        label.as_deref(),
        Some("2"),
        "expected build.labels to bake cella.test=2 into {image_name}, got {label:?}"
    );
}

/// `--label` alongside a combined Dockerfile (sub-case 1 shape): an override that
/// carries BOTH a `build.dockerfile` and `build.labels` must bake the label into
/// the built image.
///
/// Real feature resolution isn't available in this crate (no `ContainerBackend`),
/// so the "with features" case reduces to "a build override that also carries
/// labels". The override points `build.dockerfile` at the fixture's own
/// `Dockerfile` (a trivial `FROM alpine`), which is the same override structure
/// `compose_build` writes when features resolve.
#[cella_testing::runtime_test(docker, compose)]
async fn label_lands_on_dockerfile_build_image() {
    let ctx = ComposeTestContext::new("build-no-features");
    let config = load_fixture_config(&ctx.fixture_dir);
    let config_path = ctx.fixture_dir.join("devcontainer.json");
    let project = ComposeProject::from_resolved(&config, &config_path, &ctx.fixture_dir)
        .unwrap()
        .with_project_name(ctx.project_name.clone());

    let _ = tokio::process::Command::new("docker")
        .args(["volume", "create", "cella-agent"])
        .output()
        .await;

    // dockerfile + labels override (the features-build shape, minus feature
    // contexts which aren't needed to prove labels land on the image). Use a
    // relative `dockerfile` resolved against `context` — the form compose always
    // accepts (the production sub-case-2 path inherits the base dockerfile, so it
    // never emits an absolute one either).
    let mut override_cfg = plain_override(&project.primary_service);
    override_cfg.build_dockerfile = Some(std::path::PathBuf::from("Dockerfile"));
    override_cfg.build_context = Some(ctx.fixture_dir.clone());
    override_cfg.build_labels = vec!["cella.test=1".to_string()];
    let yaml = generate_override_yaml(&override_cfg);
    write_override_file(&project.override_file, &yaml).unwrap();

    let cmd = ComposeCommand::new(&project);
    cmd.build(None, false).await.expect("compose build failed");

    let image_name = format!("{}-app", project.project_name);
    let label = docker_image_label(&image_name, "cella.test").await;

    ctx.cleanup().await;
    crate::override_file::cleanup_override_file(&project.override_file);

    assert_eq!(
        label.as_deref(),
        Some("1"),
        "expected build.labels to bake cella.test=1 into {image_name}, got {label:?}"
    );
}

/// Multi-service compose: verify that runServices are started and others are not.
#[cella_testing::runtime_test(docker, compose)]
async fn multi_service_lifecycle() {
    let ctx = ComposeTestContext::new("multi-service");
    let config = load_fixture_config(&ctx.fixture_dir);
    let config_path = ctx.fixture_dir.join("devcontainer.json");
    let project = ComposeProject::from_resolved(&config, &config_path, &ctx.fixture_dir)
        .unwrap()
        .with_project_name(ctx.project_name.clone());

    // Ensure the cella-agent volume exists
    let _ = tokio::process::Command::new("docker")
        .args(["volume", "create", "cella-agent"])
        .output()
        .await;

    // Write override file
    let override_cfg = plain_override(&project.primary_service);
    let yaml = generate_override_yaml(&override_cfg);
    write_override_file(&project.override_file, &yaml).unwrap();

    let cmd = ComposeCommand::new(&project);

    // Up with specific services from runServices
    let run_services = project
        .run_services
        .as_ref()
        .expect("multi-service fixture should have runServices");
    cmd.up(Some(run_services), false)
        .await
        .expect("compose up failed");

    // Check running services
    let statuses = cmd.ps_json().await.expect("compose ps failed");

    // Both app and db should be running
    let app_running = statuses
        .iter()
        .any(|s| s.service == "app" && s.state == "running");
    let db_running = statuses
        .iter()
        .any(|s| s.service == "db" && s.state == "running");
    assert!(app_running, "app service should be running");
    assert!(db_running, "db service should be running");

    // Cleanup
    ctx.cleanup().await;
    crate::override_file::cleanup_override_file(&project.override_file);
}
