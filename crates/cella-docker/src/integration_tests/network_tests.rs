//! End-to-end tests for network cleanup against a real Docker daemon.

use std::collections::HashMap;

use bollard::Docker;
use cella_backend::RemovalOutcome;

use crate::network::{list_managed_networks, remove_network_if_orphan};

/// Create a bridge network with the given labels. Returns the network name.
async fn create_test_network(
    docker: &Docker,
    name: &str,
    labels: HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = bollard::models::NetworkCreateRequest {
        name: name.to_string(),
        driver: Some("bridge".to_string()),
        labels: Some(labels),
        ..Default::default()
    };
    docker.create_network(config).await?;
    Ok(())
}

fn test_network_name(base: &str) -> String {
    format!("{base}-{}", std::process::id())
}

/// Best-effort teardown. Used in `Drop`-style cleanup at end of each test.
async fn force_remove(docker: &Docker, name: &str) {
    let _ = docker.remove_network(name).await;
}

/// A managed orphan (no endpoints, `dev.cella.managed=true`) is removed.
#[cella_testing::runtime_test(docker)]
async fn remove_network_if_orphan_removes_managed_empty_network() {
    let docker = Docker::connect_with_local_defaults().unwrap();
    let name = test_network_name("cella-it-orphan-managed");

    force_remove(&docker, &name).await;
    create_test_network(
        &docker,
        &name,
        HashMap::from([("dev.cella.managed".to_string(), "true".to_string())]),
    )
    .await
    .expect("create test network");

    let outcome = remove_network_if_orphan(&docker, &name)
        .await
        .expect("remove_network_if_orphan");
    assert_eq!(outcome, RemovalOutcome::Removed);

    // Confirm it's actually gone.
    let err = docker.inspect_network(&name, None).await;
    assert!(
        matches!(
            err,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                ..
            })
        ),
        "expected 404 after successful removal, got {err:?}"
    );
}

/// A network missing the `dev.cella.managed` label is never touched,
/// even if it has zero endpoints — we must not remove user-owned
/// networks that happen to be idle.
#[cella_testing::runtime_test(docker)]
async fn remove_network_if_orphan_skips_unlabeled_empty_network() {
    let docker = Docker::connect_with_local_defaults().unwrap();
    let name = test_network_name("cella-it-unlabeled");

    force_remove(&docker, &name).await;
    create_test_network(&docker, &name, HashMap::new())
        .await
        .expect("create test network");

    let outcome = remove_network_if_orphan(&docker, &name)
        .await
        .expect("remove_network_if_orphan");
    assert_eq!(
        outcome,
        RemovalOutcome::SkippedInUse,
        "unlabeled network must not be removed"
    );
    // Confirm it still exists.
    let inspect = docker.inspect_network(&name, None).await;
    assert!(
        inspect.is_ok(),
        "network should still exist after skip, got {inspect:?}"
    );

    force_remove(&docker, &name).await;
}

/// A missing network reports `NotFound` (idempotent success for callers).
#[cella_testing::runtime_test(docker)]
async fn remove_network_if_orphan_not_found_is_idempotent() {
    let docker = Docker::connect_with_local_defaults().unwrap();
    let name = test_network_name("cella-it-nonexistent");
    force_remove(&docker, &name).await;

    let outcome = remove_network_if_orphan(&docker, &name)
        .await
        .expect("remove_network_if_orphan");
    assert_eq!(outcome, RemovalOutcome::NotFound);
}

/// `list_managed_networks` reports only labeled networks and omits
/// unlabeled ones.
#[cella_testing::runtime_test(docker)]
async fn list_managed_networks_filters_by_label() {
    let docker = Docker::connect_with_local_defaults().unwrap();
    let managed = test_network_name("cella-it-list-managed");
    let unlabeled = test_network_name("cella-it-list-unlabeled");

    force_remove(&docker, &managed).await;
    force_remove(&docker, &unlabeled).await;
    create_test_network(
        &docker,
        &managed,
        HashMap::from([
            ("dev.cella.managed".to_string(), "true".to_string()),
            (
                "dev.cella.repo".to_string(),
                "/tmp/integration-test".to_string(),
            ),
        ]),
    )
    .await
    .expect("create managed network");
    create_test_network(&docker, &unlabeled, HashMap::new())
        .await
        .expect("create unlabeled network");

    let listed = list_managed_networks(&docker)
        .await
        .expect("list_managed_networks");
    let names: Vec<&str> = listed.iter().map(|n| n.name.as_str()).collect();

    assert!(
        names.contains(&managed.as_str()),
        "managed network should be listed: {names:?}"
    );
    assert!(
        !names.contains(&unlabeled.as_str()),
        "unlabeled network must not be listed: {names:?}"
    );

    // Spot-check the managed entry's fields.
    let entry = listed.iter().find(|n| n.name == managed).unwrap();
    assert_eq!(entry.container_count, 0);
    assert_eq!(entry.repo_path.as_deref(), Some("/tmp/integration-test"));

    force_remove(&docker, &managed).await;
    force_remove(&docker, &unlabeled).await;
}

// The canonicalize-then-hash naming contract is covered by
// `workspace_network_name_canonicalizes_then_hashes` in
// cella-backend::network, next to the implementation.
