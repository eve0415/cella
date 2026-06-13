//! Integration test: `find_container_by_labels` finds a container by the
//! spec identity labels (`devcontainer.local_folder` + `devcontainer.config_file`).
//!
//! This proves that the reuse lookup in `cella-orchestrator` up.rs will
//! locate a container whose spec labels were stamped by `container_labels`
//! (via `lexical_absolute`) — i.e. stamp and lookup are byte-identical.

use std::collections::HashMap;
use std::path::Path;

use cella_backend::{ContainerBackend, CreateContainerOptions, lexical_absolute};

use crate::client::DockerClient;

const TEST_IMAGE: &str = "busybox:latest";

fn opts_with_spec_labels(name: &str, workspace: &Path, config: &Path) -> CreateContainerOptions {
    let local_folder = lexical_absolute(workspace).to_string_lossy().to_string();
    let config_file = lexical_absolute(config).to_string_lossy().to_string();
    let labels = HashMap::from([
        ("devcontainer.local_folder".to_string(), local_folder),
        ("devcontainer.config_file".to_string(), config_file),
    ]);
    CreateContainerOptions {
        name: name.to_string(),
        image: TEST_IMAGE.to_string(),
        labels,
        env: Vec::new(),
        remote_env: Vec::new(),
        user: None,
        workspace_folder: String::new(),
        workspace_mount: None,
        mounts: Vec::new(),
        port_bindings: HashMap::new(),
        entrypoint: None,
        cmd: Some(vec!["sleep".to_string(), "300".to_string()]),
        cap_add: Vec::new(),
        security_opt: Vec::new(),
        privileged: false,
        run_args_overrides: None,
        gpu_request: None,
    }
}

/// A container stamped with `devcontainer.local_folder` + `devcontainer.config_file`
/// (as `container_labels` does via `lexical_absolute`) is found by
/// `find_container_by_labels` using the same lexical path values — proving
/// the orchestrator reuse lookup will work end-to-end.
#[cella_testing::runtime_test(docker)]
async fn find_container_by_spec_identity_labels() {
    let Ok(client) = DockerClient::connect() else {
        return;
    };

    if client.pull_image(TEST_IMAGE).await.is_err() {
        return;
    }

    let name = format!("cella-it-spec-identity-{}", std::process::id());
    let workspace = Path::new("/tmp/spec-identity-test-workspace");
    let config = Path::new("/tmp/spec-identity-test-workspace/.devcontainer/devcontainer.json");

    let _ = client.remove_container(&name, true).await;
    let Ok(id) = client
        .create_container(&opts_with_spec_labels(&name, workspace, config))
        .await
    else {
        return;
    };

    // Build the lookup labels the same way `spec_identity_labels` does in up.rs.
    let spec_labels = [
        format!(
            "devcontainer.local_folder={}",
            lexical_absolute(workspace).to_string_lossy()
        ),
        format!(
            "devcontainer.config_file={}",
            lexical_absolute(config).to_string_lossy()
        ),
    ];

    let found = client
        .find_container_by_labels(&spec_labels)
        .await
        .expect("find_container_by_labels");

    let _ = client.remove_container(&id, true).await;

    assert!(
        found.is_some(),
        "find_container_by_labels must locate the container by spec identity labels"
    );
    assert_eq!(
        found.unwrap().id,
        id,
        "found container id must match the created container"
    );
}
