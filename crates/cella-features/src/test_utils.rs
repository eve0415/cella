//! Shared test utilities for the cella-features crate.

use crate::types::Platform;

/// Build a [`Platform`] matching the current host — used by integration tests
/// that pull real OCI images from ghcr.io.
pub fn test_platform() -> Platform {
    let architecture = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    Platform {
        os: "linux".to_string(),
        architecture: architecture.to_string(),
    }
}
