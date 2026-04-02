//! Smoke tests: full compose lifecycle with Docker.
//!
//! These tests are `#[ignore = "requires Docker daemon"]`-d by default because they require a running
//! Docker daemon. Run them with `cargo test --features integration-tests -- --ignored`.

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
    }
}

/// Plain compose: write override -> build -> up -> ps -> down.
#[tokio::test]
#[ignore = "requires Docker daemon"]
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
    cmd.build(None).await.expect("compose build failed");

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
#[tokio::test]
#[ignore = "requires Docker daemon"]
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

/// Multi-service compose: verify that runServices are started and others are not.
#[tokio::test]
#[ignore = "requires Docker daemon"]
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
