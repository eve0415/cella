//! End-to-end check of the anti-ghost seed mechanism against a real container.
//!
//! `~/.claude.json` is no longer a single-file bind mount; it is uploaded as a
//! regular file via [`DockerClient::upload_files`]. This test proves the
//! resulting file is a *regular, removable* file rather than a live mountpoint
//! — the structural property that prevents the `VirtioFS` ghost-on-atomic-replace
//! bug. (The ghost itself only reproduces on a virtualized FUSE share, so it
//! can't be triggered on a Linux CI host; the removable-regular-file invariant
//! is what we can assert portably.)

use std::collections::HashMap;

use cella_backend::{CreateContainerOptions, ExecOptions, FileToUpload};

use crate::client::DockerClient;

const TEST_IMAGE: &str = "busybox:latest";

fn minimal_opts(name: &str) -> CreateContainerOptions {
    CreateContainerOptions {
        name: name.to_string(),
        image: TEST_IMAGE.to_string(),
        labels: HashMap::new(),
        env: Vec::new(),
        remote_env: Vec::new(),
        user: None,
        workspace_folder: String::new(),
        workspace_mount: None,
        mounts: Vec::new(),
        port_bindings: HashMap::new(),
        entrypoint: None,
        // Keep the container alive long enough to exec into it.
        cmd: Some(vec!["sleep".to_string(), "300".to_string()]),
        cap_add: Vec::new(),
        security_opt: Vec::new(),
        privileged: false,
        init: false,
        run_args_overrides: None,
        gpu_request: None,
    }
}

async fn exec(client: &DockerClient, id: &str, cmd: &[&str]) -> Option<cella_backend::ExecResult> {
    client
        .exec_command(
            id,
            &ExecOptions {
                cmd: cmd.iter().map(ToString::to_string).collect(),
                user: None,
                env: None,
                working_dir: None,
            },
        )
        .await
        .ok()
}

/// Seeded `~/.claude.json` is a readable, removable regular file — not a live
/// single-file mount (which would `EBUSY` on unlink and ghost on host replace).
#[cella_testing::runtime_test(docker)]
async fn seeded_claude_json_is_regular_removable_file() {
    let Ok(client) = DockerClient::connect() else {
        return; // no Docker — runtime_test already gates this, belt-and-braces
    };

    // Best-effort setup: if the environment can't pull/create/start, skip
    // rather than fail (mirrors graceful runtime-test behavior).
    if client.pull_image(TEST_IMAGE).await.is_err() {
        return;
    }
    let name = format!("cella-it-claude-seed-{}", std::process::id());
    let _ = client.remove_container(&name, true).await;
    let Ok(id) = client.create_container(&minimal_opts(&name)).await else {
        return;
    };
    if client.start_container(&id).await.is_err() {
        let _ = client.remove_container(&id, true).await;
        return;
    }

    let path = "/root/.claude.json";
    let content = br#"{"numStartups":1,"mcpServers":{}}"#;
    let upload = vec![FileToUpload {
        path: path.to_string(),
        content: content.to_vec(),
        mode: 0o600,
    }];
    client
        .upload_files(&id, &upload)
        .await
        .expect("upload regular file into running container");

    // The file is readable with exactly the uploaded content.
    let cat = exec(&client, &id, &["cat", path]).await.expect("cat exec");
    assert_eq!(cat.exit_code, 0, "cat should succeed: {}", cat.stderr);
    assert_eq!(cat.stdout, String::from_utf8_lossy(content));

    // The decisive anti-ghost assertion: a *regular* file can be unlinked; a
    // live single-file bind mount cannot (rm fails with "Resource busy").
    let rm = exec(&client, &id, &["rm", path]).await.expect("rm exec");
    assert_eq!(
        rm.exit_code, 0,
        "seeded config must be removable (proving it is not a mountpoint): {}",
        rm.stderr
    );

    let _ = client.remove_container(&id, true).await;
}
