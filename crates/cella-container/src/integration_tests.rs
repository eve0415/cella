//! Integration tests for the Apple Container backend.
//!
//! These tests require a real Apple Container CLI installed and running on
//! macOS. They are gated behind the `integration-tests` feature and
//! `target_os = "macos"`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use cella_backend::ContainerBackend;
use cella_backend::types::{CreateContainerOptions, ExecOptions, FileToUpload, GpuRequest};

use crate::backend::AppleContainerBackend;
use crate::discovery;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// RAII guard that stops and removes a container on Drop.
struct ContainerGuard {
    cli_path: PathBuf,
    container_id: Option<String>,
}

impl ContainerGuard {
    fn new(cli_path: PathBuf) -> Self {
        Self {
            cli_path,
            container_id: None,
        }
    }

    fn set_id(&mut self, id: String) {
        self.container_id = Some(id);
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        if let Some(id) = self.container_id.take() {
            let _ = Command::new(&self.cli_path).args(["stop", &id]).output();
            let _ = Command::new(&self.cli_path).args(["rm", &id]).output();
        }
    }
}

fn setup_backend() -> (AppleContainerBackend, PathBuf) {
    let cli =
        discovery::discover().expect("Apple Container CLI must be installed for integration tests");
    let cli_path = cli.binary_path().to_path_buf();
    let backend = AppleContainerBackend::new(cli);
    (backend, cli_path)
}

fn test_container_name(test_name: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("cella-it-{test_name}-{}-{ts:08x}", std::process::id())
}

const TEST_IMAGE: &str = "alpine:3.21";

fn minimal_create_opts(name: &str) -> CreateContainerOptions {
    CreateContainerOptions {
        name: name.to_string(),
        image: TEST_IMAGE.to_string(),
        labels: HashMap::new(),
        env: Vec::new(),
        remote_env: Vec::new(),
        user: None,
        workspace_folder: "/workspace".to_string(),
        workspace_mount: None,
        mounts: Vec::new(),
        port_bindings: HashMap::new(),
        entrypoint: Some(vec!["sleep".to_string(), "300".to_string()]),
        cmd: None,
        cap_add: Vec::new(),
        security_opt: Vec::new(),
        privileged: false,
        run_args_overrides: None,
        gpu_request: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_full_lifecycle() {
    let (backend, cli_path) = setup_backend();
    let name = test_container_name("lifecycle");
    let mut guard = ContainerGuard::new(cli_path);

    // Pull image.
    backend.pull_image(TEST_IMAGE).await.unwrap();

    // Create container.
    let opts = minimal_create_opts(&name);
    let id = backend.create_container(&opts).await.unwrap();
    guard.set_id(id.clone());
    assert!(!id.is_empty(), "container ID must not be empty");

    // Start container.
    backend.start_container(&id).await.unwrap();

    // Exec a simple command.
    let exec_opts = ExecOptions {
        cmd: vec!["echo".to_string(), "hello".to_string()],
        user: None,
        env: None,
        working_dir: None,
    };
    let result = backend.exec_command(&id, &exec_opts).await.unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(result.stdout.trim().contains("hello"));

    // Inspect.
    let info = backend.inspect_container(&id).await.unwrap();
    assert_eq!(info.name, name);

    // Stop.
    backend.stop_container(&id).await.unwrap();

    // Remove.
    backend.remove_container(&id, false).await.unwrap();
    guard.container_id = None; // already removed
}

#[tokio::test(flavor = "multi_thread")]
async fn test_exec_with_env_and_workdir() {
    let (backend, cli_path) = setup_backend();
    let name = test_container_name("execenv");
    let mut guard = ContainerGuard::new(cli_path);

    let opts = minimal_create_opts(&name);
    let id = backend.create_container(&opts).await.unwrap();
    guard.set_id(id.clone());
    backend.start_container(&id).await.unwrap();

    let exec_opts = ExecOptions {
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo $MY_VAR && pwd".to_string(),
        ],
        user: None,
        env: Some(vec!["MY_VAR=integration_test_value".to_string()]),
        working_dir: Some("/tmp".to_string()),
    };
    let result = backend.exec_command(&id, &exec_opts).await.unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(
        result.stdout.contains("integration_test_value"),
        "env var not found in output: {}",
        result.stdout
    );
    assert!(
        result.stdout.contains("/tmp"),
        "working dir not reflected in output: {}",
        result.stdout
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_exec_exit_code() {
    let (backend, cli_path) = setup_backend();
    let name = test_container_name("exitcode");
    let mut guard = ContainerGuard::new(cli_path);

    let opts = minimal_create_opts(&name);
    let id = backend.create_container(&opts).await.unwrap();
    guard.set_id(id.clone());
    backend.start_container(&id).await.unwrap();

    let exec_opts = ExecOptions {
        cmd: vec!["sh".to_string(), "-c".to_string(), "exit 42".to_string()],
        user: None,
        env: None,
        working_dir: None,
    };
    let result = backend.exec_command(&id, &exec_opts).await.unwrap();
    assert_eq!(
        result.exit_code, 42,
        "expected exit code 42, got {}",
        result.exit_code
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_env_vars_in_container() {
    let (backend, cli_path) = setup_backend();
    let name = test_container_name("envvars");
    let mut guard = ContainerGuard::new(cli_path);

    let mut opts = minimal_create_opts(&name);
    opts.env = vec![
        "CELLA_TEST_A=alpha".to_string(),
        "CELLA_TEST_B=beta".to_string(),
    ];
    let id = backend.create_container(&opts).await.unwrap();
    guard.set_id(id.clone());
    backend.start_container(&id).await.unwrap();

    let exec_opts = ExecOptions {
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo $CELLA_TEST_A $CELLA_TEST_B".to_string(),
        ],
        user: None,
        env: None,
        working_dir: None,
    };
    let result = backend.exec_command(&id, &exec_opts).await.unwrap();
    assert_eq!(result.exit_code, 0);
    let output = result.stdout.trim();
    assert!(
        output.contains("alpha") && output.contains("beta"),
        "expected env vars in output, got: {output}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_labels_roundtrip() {
    let (backend, cli_path) = setup_backend();
    let name = test_container_name("labels");
    let mut guard = ContainerGuard::new(cli_path);

    let mut opts = minimal_create_opts(&name);
    opts.labels
        .insert("dev.cella.tool".to_string(), "cella".to_string());
    opts.labels
        .insert("dev.cella.test_key".to_string(), "test_value".to_string());

    let id = backend.create_container(&opts).await.unwrap();
    guard.set_id(id.clone());

    let info = backend.inspect_container(&id).await.unwrap();
    assert_eq!(
        info.labels.get("dev.cella.tool").map(String::as_str),
        Some("cella"),
        "dev.cella.tool label missing or wrong: {:?}",
        info.labels
    );
    assert_eq!(
        info.labels.get("dev.cella.test_key").map(String::as_str),
        Some("test_value"),
        "dev.cella.test_key label missing or wrong: {:?}",
        info.labels
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_list_cella_containers() {
    let (backend, cli_path) = setup_backend();
    let name = test_container_name("list");
    let mut guard = ContainerGuard::new(cli_path);

    let mut opts = minimal_create_opts(&name);
    opts.labels
        .insert("dev.cella.tool".to_string(), "cella".to_string());

    let id = backend.create_container(&opts).await.unwrap();
    guard.set_id(id.clone());
    backend.start_container(&id).await.unwrap();

    // List running only should include our container.
    let running = backend.list_cella_containers(true).await.unwrap();
    assert!(
        running.iter().any(|c| c.name == name),
        "expected container {name} in running list, got: {:?}",
        running.iter().map(|c| &c.name).collect::<Vec<_>>()
    );

    // Stop it and verify it is excluded from running-only list.
    backend.stop_container(&id).await.unwrap();
    let running_after = backend.list_cella_containers(true).await.unwrap();
    assert!(
        !running_after.iter().any(|c| c.name == name),
        "stopped container {name} should not appear in running-only list"
    );

    // But should appear in the full list.
    let all = backend.list_cella_containers(false).await.unwrap();
    assert!(
        all.iter().any(|c| c.name == name),
        "expected container {name} in full list"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_file_upload() {
    let (backend, cli_path) = setup_backend();
    let name = test_container_name("upload");
    let mut guard = ContainerGuard::new(cli_path);

    let opts = minimal_create_opts(&name);
    let id = backend.create_container(&opts).await.unwrap();
    guard.set_id(id.clone());
    backend.start_container(&id).await.unwrap();

    let test_content = b"hello from integration test\n";
    let files = vec![FileToUpload {
        path: "/tmp/cella-test-upload.txt".to_string(),
        content: test_content.to_vec(),
        mode: 0o644,
    }];

    backend.upload_files(&id, &files).await.unwrap();

    // Verify the file was written.
    let exec_opts = ExecOptions {
        cmd: vec!["cat".to_string(), "/tmp/cella-test-upload.txt".to_string()],
        user: None,
        env: None,
        working_dir: None,
    };
    let result = backend.exec_command(&id, &exec_opts).await.unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout.trim(), "hello from integration test");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_find_container_by_workspace_path() {
    let (backend, cli_path) = setup_backend();
    let name = test_container_name("findws");
    let mut guard = ContainerGuard::new(cli_path);

    // Use a unique workspace path to avoid collisions with other tests.
    let workspace_path = format!("/tmp/cella-test-workspace-{}", std::process::id());

    // Create the directory so canonicalize works predictably.
    // On macOS /tmp -> /private/tmp, so we need the canonical path.
    tokio::fs::create_dir_all(&workspace_path).await.unwrap();
    let canonical = std::fs::canonicalize(&workspace_path).unwrap();

    let mut opts = minimal_create_opts(&name);
    opts.labels
        .insert("dev.cella.tool".to_string(), "cella".to_string());
    opts.labels.insert(
        "dev.cella.workspace_path".to_string(),
        canonical.to_string_lossy().to_string(),
    );

    let id = backend.create_container(&opts).await.unwrap();
    guard.set_id(id.clone());

    let found = backend.find_container(canonical.as_path()).await.unwrap();
    assert!(
        found.is_some(),
        "expected to find container by workspace path"
    );
    assert_eq!(found.unwrap().name, name);

    // Clean up temp dir.
    let _ = tokio::fs::remove_dir_all(&workspace_path).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_inspect_image_details() {
    let (backend, _cli_path) = setup_backend();

    backend.pull_image(TEST_IMAGE).await.unwrap();

    let details = backend.inspect_image_details(TEST_IMAGE).await.unwrap();
    // Alpine default user is root.
    assert_eq!(details.user, "root", "expected root user for alpine image");
    // Alpine should have at least PATH in its env.
    assert!(
        details.env.iter().any(|e| e.starts_with("PATH=")),
        "expected PATH in image env, got: {:?}",
        details.env
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_privileged_warns_not_errors() {
    let (backend, cli_path) = setup_backend();
    let name = test_container_name("priv");
    let mut guard = ContainerGuard::new(cli_path);

    let mut opts = minimal_create_opts(&name);
    opts.privileged = true;

    // Should succeed (warning emitted, not an error).
    let id = backend.create_container(&opts).await.unwrap();
    guard.set_id(id.clone());
    assert!(!id.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cap_add_warns_not_errors() {
    let (backend, cli_path) = setup_backend();
    let name = test_container_name("capadd");
    let mut guard = ContainerGuard::new(cli_path);

    let mut opts = minimal_create_opts(&name);
    opts.cap_add = vec!["SYS_PTRACE".to_string(), "NET_ADMIN".to_string()];

    let id = backend.create_container(&opts).await.unwrap();
    guard.set_id(id.clone());
    assert!(!id.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_security_opt_warns_not_errors() {
    let (backend, cli_path) = setup_backend();
    let name = test_container_name("secopt");
    let mut guard = ContainerGuard::new(cli_path);

    let mut opts = minimal_create_opts(&name);
    opts.security_opt = vec!["seccomp=unconfined".to_string()];

    let id = backend.create_container(&opts).await.unwrap();
    guard.set_id(id.clone());
    assert!(!id.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gpu_request_warns_not_errors() {
    let (backend, cli_path) = setup_backend();
    let name = test_container_name("gpu");
    let mut guard = ContainerGuard::new(cli_path);

    let mut opts = minimal_create_opts(&name);
    opts.gpu_request = Some(GpuRequest::All);

    let id = backend.create_container(&opts).await.unwrap();
    guard.set_id(id.clone());
    assert!(!id.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_container_restart() {
    let (backend, cli_path) = setup_backend();
    let name = test_container_name("restart");
    let mut guard = ContainerGuard::new(cli_path);

    let opts = minimal_create_opts(&name);
    let id = backend.create_container(&opts).await.unwrap();
    guard.set_id(id.clone());

    // Start → stop → start again.
    backend.start_container(&id).await.unwrap();
    backend.stop_container(&id).await.unwrap();
    backend.start_container(&id).await.unwrap();

    // Should still be functional after restart.
    let exec_opts = ExecOptions {
        cmd: vec!["echo".to_string(), "after-restart".to_string()],
        user: None,
        env: None,
        working_dir: None,
    };
    let result = backend.exec_command(&id, &exec_opts).await.unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(result.stdout.contains("after-restart"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_exec_stderr_capture() {
    let (backend, cli_path) = setup_backend();
    let name = test_container_name("stderr");
    let mut guard = ContainerGuard::new(cli_path);

    let opts = minimal_create_opts(&name);
    let id = backend.create_container(&opts).await.unwrap();
    guard.set_id(id.clone());
    backend.start_container(&id).await.unwrap();

    let exec_opts = ExecOptions {
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo err_msg >&2".to_string(),
        ],
        user: None,
        env: None,
        working_dir: None,
    };
    let result = backend.exec_command(&id, &exec_opts).await.unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(
        result.stderr.contains("err_msg"),
        "expected stderr to contain err_msg, got: {}",
        result.stderr
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_multiple_file_uploads() {
    let (backend, cli_path) = setup_backend();
    let name = test_container_name("multiup");
    let mut guard = ContainerGuard::new(cli_path);

    let opts = minimal_create_opts(&name);
    let id = backend.create_container(&opts).await.unwrap();
    guard.set_id(id.clone());
    backend.start_container(&id).await.unwrap();

    let files = vec![
        FileToUpload {
            path: "/tmp/cella-test-a.txt".to_string(),
            content: b"file-a-content".to_vec(),
            mode: 0o644,
        },
        FileToUpload {
            path: "/tmp/cella-test-b.txt".to_string(),
            content: b"file-b-content".to_vec(),
            mode: 0o600,
        },
    ];

    backend.upload_files(&id, &files).await.unwrap();

    // Verify both files.
    for (path, expected) in [
        ("/tmp/cella-test-a.txt", "file-a-content"),
        ("/tmp/cella-test-b.txt", "file-b-content"),
    ] {
        let exec_opts = ExecOptions {
            cmd: vec!["cat".to_string(), path.to_string()],
            user: None,
            env: None,
            working_dir: None,
        };
        let result = backend.exec_command(&id, &exec_opts).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout.trim(), expected, "file {path} mismatch");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_remove_nonexistent_container() {
    let (backend, _cli_path) = setup_backend();

    let result = backend
        .remove_container("nonexistent-container-id-12345", false)
        .await;
    assert!(
        result.is_err(),
        "removing a nonexistent container should fail"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_workspace_folder_label_set() {
    let (backend, cli_path) = setup_backend();
    let name = test_container_name("wslabel");
    let mut guard = ContainerGuard::new(cli_path);

    let mut opts = minimal_create_opts(&name);
    opts.labels
        .insert("dev.cella.tool".to_string(), "cella".to_string());

    let id = backend.create_container(&opts).await.unwrap();
    guard.set_id(id.clone());

    let info = backend.inspect_container(&id).await.unwrap();
    // workspace_folder from create opts should be reflected
    assert_eq!(
        info.labels
            .get("dev.cella.workspace_folder")
            .map(String::as_str),
        Some("/workspace"),
    );
}
