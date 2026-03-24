//! Docker Compose orchestration for cella devcontainers.
//!
//! This crate handles:
//! - Parsing Docker Compose YAML files (minimal subset for validation)
//! - Generating override compose files for cella customizations
//! - Shelling out to the `docker compose` V2 CLI
//! - Discovering compose-managed containers via Docker labels
//! - Computing multi-file change detection hashes

pub mod cli;
pub mod discovery;
pub mod error;
pub mod hash;
pub mod override_file;
pub mod parse;
pub mod project;

pub use cli::ComposeCommand;
pub use error::CellaComposeError;
pub use override_file::OverrideConfig;
pub use project::{ComposeProject, ShutdownAction};
