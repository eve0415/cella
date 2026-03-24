//! Compose container discovery via Docker labels.
//!
//! Docker Compose V2 sets standard labels on managed containers:
//! - `com.docker.compose.project` — the compose project name
//! - `com.docker.compose.service` — the service name within the project

use std::collections::HashMap;
use std::hash::BuildHasher;

/// Label key for the compose project name.
pub const LABEL_COMPOSE_PROJECT: &str = "com.docker.compose.project";

/// Label key for the compose service name.
pub const LABEL_COMPOSE_SERVICE: &str = "com.docker.compose.service";

/// Cella-specific label indicating this is a compose-managed devcontainer.
pub const LABEL_CELLA_COMPOSE_PROJECT: &str = "dev.cella.compose_project";

/// Cella-specific label for the primary service name.
pub const LABEL_CELLA_PRIMARY_SERVICE: &str = "dev.cella.primary_service";

/// Check if a container is part of a compose project by inspecting its labels.
pub fn is_compose_container<S: BuildHasher>(labels: &HashMap<String, String, S>) -> bool {
    labels.contains_key(LABEL_COMPOSE_PROJECT)
}

/// Extract the compose project name from container labels.
pub fn compose_project_from_labels<S: BuildHasher>(
    labels: &HashMap<String, String, S>,
) -> Option<&str> {
    labels.get(LABEL_COMPOSE_PROJECT).map(String::as_str)
}

/// Extract the compose service name from container labels.
pub fn compose_service_from_labels<S: BuildHasher>(
    labels: &HashMap<String, String, S>,
) -> Option<&str> {
    labels.get(LABEL_COMPOSE_SERVICE).map(String::as_str)
}

/// Check if this container is the primary (cella-managed) service.
pub fn is_primary_service<S: BuildHasher>(labels: &HashMap<String, String, S>) -> bool {
    labels.contains_key(LABEL_CELLA_COMPOSE_PROJECT)
}
