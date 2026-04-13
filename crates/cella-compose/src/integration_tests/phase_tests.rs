//! Phase tests: verify compose override generation, combined Dockerfiles,
//! and `additional_contexts` without requiring Docker.
//!
//! These tests exercise the configuration-generation pipeline in isolation,
//! ensuring override files are correct before any `docker compose` call.

use super::helpers::{ComposeTestContext, create_test_feature, load_fixture_config};
use crate::dockerfile::{FEATURES_TARGET_STAGE, ensure_stage_named, synthetic_dockerfile};
use crate::override_file::{OverrideConfig, generate_override_yaml, write_override_file};
use crate::project::ComposeProject;
use std::collections::{BTreeMap, BTreeSet};

/// Verify that the override file exists on disk after `write_override_file`.
#[test]
fn override_file_exists_before_compose_uses_it() {
    let ctx = ComposeTestContext::new("plain-compose");
    let config = load_fixture_config(&ctx.fixture_dir);
    let config_path = ctx.fixture_dir.join("devcontainer.json");
    let project = ComposeProject::from_resolved(&config, &config_path, &ctx.fixture_dir)
        .unwrap()
        .with_project_name(ctx.project_name.clone());

    let override_cfg = OverrideConfig {
        primary_service: project.primary_service.clone(),
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
        extra_volumes: Vec::new(),
        base_compose_volumes: BTreeSet::new(),
    };

    let yaml = generate_override_yaml(&override_cfg);
    write_override_file(&project.override_file, &yaml).unwrap();

    assert!(
        project.override_file.exists(),
        "override file should exist at {}",
        project.override_file.display()
    );

    // Verify content has the expected service name
    let written = std::fs::read_to_string(&project.override_file).unwrap();
    assert!(
        written.contains("app:"),
        "override should reference the primary service"
    );
    assert!(
        written.contains("cella-agent"),
        "override should include agent volume"
    );

    // Cleanup
    crate::override_file::cleanup_override_file(&project.override_file);
}

/// A `ComposeCommand::without_override` should not include the override
/// file path in its compose files. We verify this by checking that the
/// project struct exposes the right `compose_files` (no override) vs
/// the `override_file` path being separate.
#[test]
fn without_override_excludes_override_path() {
    let ctx = ComposeTestContext::new("plain-compose");
    let config = load_fixture_config(&ctx.fixture_dir);
    let config_path = ctx.fixture_dir.join("devcontainer.json");
    let project = ComposeProject::from_resolved(&config, &config_path, &ctx.fixture_dir)
        .unwrap()
        .with_project_name(ctx.project_name.clone());

    // The override file should NOT be in the compose_files list
    let override_in_compose_files = project
        .compose_files
        .iter()
        .any(|p| p.to_string_lossy().contains("docker-compose.cella.yml"));
    assert!(
        !override_in_compose_files,
        "compose_files should not contain the override file"
    );

    // The override file should exist as a separate path
    assert!(
        project
            .override_file
            .to_string_lossy()
            .contains("docker-compose.cella.yml"),
        "override_file should reference the cella override"
    );

    // compose_files should contain only user compose files
    assert!(
        !project.compose_files.is_empty(),
        "compose_files should contain at least the user compose file"
    );
    for f in &project.compose_files {
        assert!(
            f.to_string_lossy().contains("docker-compose.yml"),
            "compose_files should reference user compose files, got: {}",
            f.display()
        );
    }
}

/// For a build-based service, the combined Dockerfile should place the
/// global ARG before the first FROM.
#[test]
fn combined_dockerfile_global_arg_before_from() {
    let ctx = ComposeTestContext::new("build-based-features");

    // Read the fixture Dockerfile
    let dockerfile_path = ctx.fixture_dir.join("Dockerfile");
    let dockerfile_content = std::fs::read_to_string(&dockerfile_path).unwrap();

    // Ensure the last FROM has a stage name
    let (named_content, stage_name) = ensure_stage_named(&dockerfile_content, None).unwrap();

    // Generate a synthetic feature Dockerfile snippet
    let feature_dockerfile = format!(
        "FROM $_DEV_CONTAINERS_BASE_IMAGE AS {FEATURES_TARGET_STAGE}\nUSER root\nRUN echo feature-install\n"
    );

    let combined = crate::dockerfile::generate_combined_dockerfile(
        &named_content,
        &feature_dockerfile,
        &stage_name,
        "root",
    );

    // Global ARG must appear before first FROM
    let arg_pos = combined
        .find("ARG _DEV_CONTAINERS_BASE_IMAGE=")
        .expect("global ARG not found in combined Dockerfile");
    let from_pos = combined
        .find("FROM ")
        .expect("no FROM in combined Dockerfile");
    assert!(
        arg_pos < from_pos,
        "global ARG (at byte {arg_pos}) must precede first FROM (at byte {from_pos})"
    );

    // Feature target stage should appear
    assert!(
        combined.contains(FEATURES_TARGET_STAGE),
        "combined Dockerfile should contain the features target stage"
    );
}

/// For image-only services with features, the override should use
/// `additional_contexts` when configured.
#[test]
fn additional_contexts_in_override_for_features() {
    let ctx = ComposeTestContext::new("image-only-features");
    let config = load_fixture_config(&ctx.fixture_dir);
    let config_path = ctx.fixture_dir.join("devcontainer.json");
    let project = ComposeProject::from_resolved(&config, &config_path, &ctx.fixture_dir)
        .unwrap()
        .with_project_name(ctx.project_name.clone());

    // Build a synthetic Dockerfile for the image-only service
    let (_synth_content, _stage_name) = synthetic_dockerfile("alpine:3.21");

    // Create a test feature and use its directory as the content source
    create_test_feature(&ctx.fixture_dir, "test-feature");
    let feature_content_dir = ctx.fixture_dir.join("test-feature");

    let mut additional_contexts = BTreeMap::new();
    additional_contexts.insert(
        "dev_containers_feature_content_source".to_string(),
        feature_content_dir.clone(),
    );

    let override_cfg = OverrideConfig {
        primary_service: project.primary_service,
        image_override: Some(format!("cella-img-{}", ctx.project_name)),
        override_command: false,
        agent_volume_name: "cella-agent".to_string(),
        agent_volume_target: "/cella".to_string(),
        extra_env: Vec::new(),
        extra_labels: BTreeMap::new(),
        build_dockerfile: Some(ctx.fixture_dir.join("Dockerfile.combined")),
        build_target: Some(FEATURES_TARGET_STAGE.to_string()),
        build_context: Some(ctx.fixture_dir.join(".build-context")),
        additional_contexts,
        build_secrets: Vec::new(),
        extra_volumes: Vec::new(),
        base_compose_volumes: BTreeSet::new(),
    };

    let yaml = generate_override_yaml(&override_cfg);

    assert!(
        yaml.contains("additional_contexts:"),
        "override YAML should contain additional_contexts section"
    );
    assert!(
        yaml.contains("dev_containers_feature_content_source="),
        "override YAML should reference the feature content source"
    );
    assert!(
        yaml.contains(&feature_content_dir.to_string_lossy().to_string()),
        "override YAML should contain the feature content directory path"
    );
    assert!(
        yaml.contains("build:"),
        "override YAML should contain a build section"
    );
    assert!(
        yaml.contains(FEATURES_TARGET_STAGE),
        "override YAML should reference the features target stage"
    );
}
