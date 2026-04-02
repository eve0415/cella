//! Docker Compose orchestration for cella devcontainers.
//!
//! This crate handles:
//! - Parsing Docker Compose YAML files (minimal subset for validation)
//! - Generating override compose files for cella customizations
//! - Shelling out to the `docker compose` V2 CLI
//! - Discovering compose-managed containers via Docker labels
//! - Computing multi-file change detection hashes

pub mod cli;
pub mod config;
pub mod discovery;
pub mod dockerfile;
pub mod error;
pub mod hash;
pub mod override_file;
pub mod parse;
pub mod project;

pub use cli::{ComposeCommand, ComposeServiceStatus, check_compose_features_support};
pub use config::{ResolvedComposeConfig, ServiceBuildInfo, extract_service_build_info};
pub use dockerfile::{
    AUTO_STAGE_NAME, FEATURES_TARGET_STAGE, ensure_stage_named, generate_combined_dockerfile,
    synthetic_dockerfile,
};
pub use error::CellaComposeError;
pub use override_file::{ComposeSecret, OverrideConfig};
pub use project::{ComposeProject, ShutdownAction};

#[cfg(all(test, feature = "integration-tests"))]
mod integration_tests;
