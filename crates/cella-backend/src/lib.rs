pub mod agent;
pub mod error;
pub mod lifecycle;
pub mod mount;
pub mod names;
pub mod network;
pub mod progress;
pub mod resolve;
pub mod traits;
pub mod types;

pub use agent::agent_env_vars;
pub use error::BackendError;
pub use lifecycle::{
    LifecycleContext, OutputCallback, ParsedLifecycle, parse_lifecycle_command, run_lifecycle_phase,
};
pub use mount::{MountKind, MountSpec};
pub use names::{
    BACKEND_LABEL, compose_labels, compose_project_name, container_labels, container_name,
    image_name, image_name_with_features, worktree_labels,
};
pub use network::{ManagedNetwork, RemovalOutcome};
pub use resolve::ContainerTarget;
pub use traits::{BackendCapabilities, BoxFuture, ContainerBackend, Platform};
pub use types::{
    BackendKind, BuildOptions, BuildSecret, ContainerInfo, ContainerState, CreateContainerOptions,
    DeviceSpec, ExecOptions, ExecResult, FileToUpload, GpuRequest, ImageDetails,
    InteractiveExecOptions, MountConfig, MountInfo, PortBinding, PortForward, RunArgsOverrides,
    UlimitSpec,
};
