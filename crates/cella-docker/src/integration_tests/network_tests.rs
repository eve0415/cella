//! End-to-end tests for network cleanup against a real Docker daemon.

use std::collections::HashMap;

use bollard::Docker;
use cella_backend::RemovalOutcome;

use crate::network::{
    list_managed_networks, remove_network_if_orphan, repo_network_name, workspace_network_name,
};

/// Connect to Docker using local defaults. Skip the test if unreachable
/// so a failed daemon doesn't look like a cella regression.
async fn docker_or_skip(test_name: &str) -> Option<Docker> {
    match Docker::connect_with_local_defaults() {
        Ok(docker) => match docker.ping().await {
            Ok(_) => Some(docker),
            Err(e) => {
                eprintln!("{test_name}: Docker ping failed ({e}); skipping");
                None
            }
        },
        Err(e) => {
            eprintln!("{test_name}: Docker unreachable ({e}); skipping");
            None
        }
    }
}

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

/// Best-effort teardown. Used in `Drop`-style cleanup at end of each test.
async fn force_remove(docker: &Docker, name: &str) {
    let _ = docker.remove_network(name).await;
}

/// A managed orphan (no endpoints, `dev.cella.managed=true`) is removed.
#[tokio::test]
#[ignore = "requires Docker daemon"]
async fn remove_network_if_orphan_removes_managed_empty_network() {
    let Some(docker) =
        docker_or_skip("remove_network_if_orphan_removes_managed_empty_network").await
    else {
        return;
    };
    let name = "cella-integration-orphan-managed-empty";

    force_remove(&docker, name).await;
    create_test_network(
        &docker,
        name,
        HashMap::from([("dev.cella.managed".to_string(), "true".to_string())]),
    )
    .await
    .expect("create test network");

    let outcome = remove_network_if_orphan(&docker, name)
        .await
        .expect("remove_network_if_orphan");
    assert_eq!(outcome, RemovalOutcome::Removed);

    // Confirm it's actually gone.
    let err = docker.inspect_network(name, None).await;
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
#[tokio::test]
#[ignore = "requires Docker daemon"]
async fn remove_network_if_orphan_skips_unlabeled_empty_network() {
    let Some(docker) =
        docker_or_skip("remove_network_if_orphan_skips_unlabeled_empty_network").await
    else {
        return;
    };
    let name = "cella-integration-unlabeled-empty";

    force_remove(&docker, name).await;
    create_test_network(&docker, name, HashMap::new())
        .await
        .expect("create test network");

    let outcome = remove_network_if_orphan(&docker, name)
        .await
        .expect("remove_network_if_orphan");
    assert_eq!(
        outcome,
        RemovalOutcome::SkippedInUse,
        "unlabeled network must not be removed"
    );
    // Confirm it still exists.
    let inspect = docker.inspect_network(name, None).await;
    assert!(
        inspect.is_ok(),
        "network should still exist after skip, got {inspect:?}"
    );

    force_remove(&docker, name).await;
}

/// A missing network reports `NotFound` (idempotent success for callers).
#[tokio::test]
#[ignore = "requires Docker daemon"]
async fn remove_network_if_orphan_not_found_is_idempotent() {
    let Some(docker) = docker_or_skip("remove_network_if_orphan_not_found_is_idempotent").await
    else {
        return;
    };
    let name = "cella-integration-does-not-exist-xyz-123";
    force_remove(&docker, name).await; // extra safety

    let outcome = remove_network_if_orphan(&docker, name)
        .await
        .expect("remove_network_if_orphan");
    assert_eq!(outcome, RemovalOutcome::NotFound);
}

/// `list_managed_networks` reports only labeled networks and omits
/// unlabeled ones.
#[tokio::test]
#[ignore = "requires Docker daemon"]
async fn list_managed_networks_filters_by_label() {
    let Some(docker) = docker_or_skip("list_managed_networks_filters_by_label").await else {
        return;
    };
    let managed = "cella-integration-list-managed-xyz";
    let unlabeled = "cella-integration-list-unlabeled-xyz";

    force_remove(&docker, managed).await;
    force_remove(&docker, unlabeled).await;
    create_test_network(
        &docker,
        managed,
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
    create_test_network(&docker, unlabeled, HashMap::new())
        .await
        .expect("create unlabeled network");

    let listed = list_managed_networks(&docker)
        .await
        .expect("list_managed_networks");
    let names: Vec<&str> = listed.iter().map(|n| n.name.as_str()).collect();

    assert!(
        names.contains(&managed),
        "managed network should be listed: {names:?}"
    );
    assert!(
        !names.contains(&unlabeled),
        "unlabeled network must not be listed: {names:?}"
    );

    // Spot-check the managed entry's fields.
    let entry = listed.iter().find(|n| n.name == managed).unwrap();
    assert_eq!(entry.container_count, 0);
    assert_eq!(entry.repo_path.as_deref(), Some("/tmp/integration-test"));

    force_remove(&docker, managed).await;
    force_remove(&docker, unlabeled).await;
}

/// `workspace_network_name` matches `repo_network_name` for the same
/// canonicalized path — the contract `cella down --rm` relies on to
/// find the network it's trying to remove.
#[tokio::test]
#[ignore = "requires Docker daemon"]
async fn workspace_network_name_matches_repo_network_name_for_real_path() {
    let tmpdir =
        std::env::temp_dir().join(format!("cella-integration-wsroot-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmpdir);
    let canonical = tmpdir.canonicalize().expect("canonicalize tmpdir");

    let via_workspace = workspace_network_name(&tmpdir);
    let via_repo = repo_network_name(&canonical);
    assert_eq!(
        via_workspace, via_repo,
        "workspace_network_name should canonicalize then hash"
    );

    let _ = std::fs::remove_dir_all(&tmpdir);
}
