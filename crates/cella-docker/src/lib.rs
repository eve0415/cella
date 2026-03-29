pub mod client;
pub mod config_map;
pub mod container;
pub mod discovery;
mod docker_api_impl;
mod error;
pub mod exec;
pub mod image;
pub mod lifecycle;
pub mod names;
pub mod network;
pub mod resolve;
pub mod uid;
pub mod upload;
pub mod volume;

pub use client::DockerClient;
pub use config_map::to_bollard_config;
pub use error::CellaDockerError;
pub use lifecycle::{
    LifecycleContext, ParsedLifecycle, parse_lifecycle_command, run_lifecycle_phase,
};
pub use resolve::ContainerTarget;
pub use uid::update_remote_user_uid;

// Re-export types from cella-backend for backward compatibility.
pub use cella_backend::names::{
    compose_labels, compose_project_name, container_labels, container_name, image_name,
    image_name_with_features, worktree_labels,
};
pub use cella_backend::{
    BackendError, BackendKind, BoxFuture, BuildOptions, ComposeBackend, ContainerBackend,
    ContainerInfo, ContainerState, CreateContainerOptions, ExecOptions, ExecResult, FileToUpload,
    ImageDetails, InteractiveExecOptions, MountConfig, MountInfo, PortBinding,
};
