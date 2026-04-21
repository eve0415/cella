pub mod client;
pub mod config_map;
pub mod container;
pub mod discovery;
mod docker_api_impl;
mod error;
pub mod exec;
pub mod image;
pub mod network;
pub mod upload;
pub mod volume;

pub use client::DockerClient;
pub use config_map::to_bollard_config;
pub use error::CellaDockerError;

#[cfg(all(test, feature = "integration-tests"))]
mod integration_tests;
