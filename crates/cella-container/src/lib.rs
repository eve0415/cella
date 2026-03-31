//! Apple Container backend for cella.
//!
//! This crate implements `cella_backend::ContainerBackend` by driving
//! the Apple `container` CLI binary. It is an EXPERIMENTAL backend —
//! the CLI output format is pre-1.0 and may change between releases.

pub mod backend;
pub mod discovery;
pub mod sdk;

pub use backend::AppleContainerBackend;

#[cfg(all(test, feature = "integration-tests", target_os = "macos"))]
mod integration_tests;
