use std::path::Path;
use std::sync::Arc;

use cella_daemon_client::{DaemonClient, DaemonClientError};
use cella_protocol::{ManagementRequest, ManagementResponse};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex;

fn spawn_mock_daemon(
    socket_path: &Path,
    response: ManagementResponse,
) -> (
    Arc<Mutex<Option<ManagementRequest>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = UnixListener::bind(socket_path).expect("bind mock daemon socket");
    let received = Arc::new(Mutex::new(None));
    let received_for_task = received.clone();
    let task = tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        if reader.read_line(&mut line).await.is_ok()
            && let Ok(request) = serde_json::from_str(line.trim())
        {
            *received_for_task.lock().await = Some(request);
        }
        let mut json = serde_json::to_string(&response).unwrap();
        json.push('\n');
        let _ = writer.write_all(json.as_bytes()).await;
        let _ = writer.flush().await;
    });
    (received, task)
}

#[tokio::test]
async fn request_sends_newline_delimited_management_json() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("daemon.sock");
    let (received, task) = spawn_mock_daemon(&socket_path, ManagementResponse::Pong);

    let response = DaemonClient::new(&socket_path)
        .request(&ManagementRequest::Ping)
        .await
        .unwrap();

    assert!(matches!(response, ManagementResponse::Pong));
    assert!(matches!(
        received.lock().await.as_ref(),
        Some(ManagementRequest::Ping)
    ));

    task.abort();
}

#[tokio::test]
async fn query_status_rejects_unexpected_response_variant() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("daemon.sock");
    let (_received, task) = spawn_mock_daemon(&socket_path, ManagementResponse::Pong);

    let err = DaemonClient::new(&socket_path)
        .query_status()
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        DaemonClientError::UnexpectedResponse {
            expected: "status",
            ..
        }
    ));

    task.abort();
}
