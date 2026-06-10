//! Apple Container backend for cella.
//!
//! This crate implements `cella_backend::ContainerBackend` by driving
//! the Apple `container` CLI binary (1.0.0 or newer; the stable
//! structured-output shapes shipped with 1.0.0).

pub mod backend;
pub mod discovery;
pub mod sdk;

pub use backend::AppleContainerBackend;

#[cfg(all(test, target_os = "macos"))]
mod integration_tests;
#[cfg(test)]
mod test_support;
