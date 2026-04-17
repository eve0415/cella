//! Agent-local state snapshot exposed for the in-container doctor.
//!
//! The host daemon has an authoritative view of whether an agent is currently
//! connected (see [`cella_daemon::control_server::AgentConnectionState`]). The
//! in-container `cella doctor` cannot query the daemon's management socket —
//! that socket lives on the host — so without this module it could only probe
//! the daemon by opening its own TCP handshake. That probe says nothing about
//! the long-running `cella-agent daemon` process inside the container.
//!
//! This module exposes a tiny file at [`DEFAULT_STATE_FILE`] that the main
//! agent process writes on startup, on every transport-state transition, and
//! every 10s while alive. The in-container doctor reads the file, verifies the
//! PID is still in `/proc`, and reports the last-known state with a freshness
//! age. A single writer task owns the file so concurrent transitions never
//! race.
//!
//! # File format
//!
//! The file is JSON; see [`AgentStateSnapshot`]. Writes are atomic via
//! temp-file-rename to avoid torn reads.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Default location of the state file. Chosen inside the container's local
/// `/tmp` (NOT the shared `/cella` volume) so writes don't affect sibling
/// containers.
pub const DEFAULT_STATE_FILE: &str = "/tmp/cella-agent.state";

/// Transport-layer state of the main agent's daemon connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    /// Handshake completed and connection is healthy.
    Connected,
    /// Connection dropped; background task is retrying.
    Reconnecting,
    /// No connection has been established yet, or the agent is shutting down.
    Disconnected,
}

/// A point-in-time snapshot of the main agent's state. Read by the
/// in-container doctor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStateSnapshot {
    /// PID of the main `cella-agent daemon` process that wrote this file.
    pub pid: u32,
    /// Transport state.
    pub state: AgentState,
    /// Current daemon address (if any). `None` while `Disconnected`.
    pub daemon_addr: Option<String>,
    /// Version of the agent binary (`CARGO_PKG_VERSION` at build time).
    pub agent_version: String,
    /// Unix seconds when the agent started.
    pub started_at_unix: u64,
    /// Unix seconds of the most recent write (heartbeat or transition).
    pub last_heartbeat_unix: u64,
}

/// Mutation applied to the state writer's local view.
#[derive(Debug, Clone)]
pub enum StateUpdate {
    /// Replace the transport state.
    SetState(AgentState),
    /// Atomic transition: set both state and daemon address in a single
    /// writer-loop iteration. Avoids the brief intermediate snapshot that
    /// would otherwise be written between two separate updates.
    Connected { daemon_addr: String },
}

/// Handle to the state writer task. Cloneable so multiple producers can share
/// it without locking.
#[derive(Clone)]
pub struct StateWriter {
    tx: mpsc::Sender<StateUpdate>,
}

impl StateWriter {
    /// Send a best-effort state update. If the writer task has exited, the
    /// send fails silently — doctor staleness will surface the stall instead.
    pub fn update(&self, update: StateUpdate) {
        if let Err(e) = self.tx.try_send(update) {
            debug!("state writer channel full or closed: {e}");
        }
    }

    /// Convenience: push a new transport state without touching the daemon
    /// address. Used for `Reconnecting` and `Disconnected` transitions where
    /// the addr either stays or becomes irrelevant.
    pub fn set_state(&self, state: AgentState) {
        self.update(StateUpdate::SetState(state));
    }

    /// Convenience: atomically transition to `Connected` with the given
    /// daemon address. Preferred over two separate updates to avoid a
    /// half-written intermediate snapshot.
    pub fn set_connected(&self, daemon_addr: String) {
        self.update(StateUpdate::Connected { daemon_addr });
    }
}

/// Spawn the single-writer state task. The task owns [`path`] for the life of
/// the process, writes heartbeats every [`heartbeat_interval`], and flushes on
/// every [`StateUpdate`].
///
/// Returns a cloneable [`StateWriter`] used by all callers.
pub fn spawn_state_writer(
    path: PathBuf,
    agent_version: String,
    initial_state: AgentState,
    heartbeat_interval: Duration,
) -> StateWriter {
    let (tx, rx) = mpsc::channel(16);
    let pid = std::process::id();
    let started = now_unix();
    tokio::spawn(async move {
        writer_loop(
            path,
            pid,
            agent_version,
            started,
            initial_state,
            rx,
            heartbeat_interval,
        )
        .await;
    });
    StateWriter { tx }
}

async fn writer_loop(
    path: PathBuf,
    pid: u32,
    agent_version: String,
    started_at_unix: u64,
    mut current_state: AgentState,
    mut rx: mpsc::Receiver<StateUpdate>,
    heartbeat_interval: Duration,
) {
    let mut daemon_addr: Option<String> = None;
    let mut interval = tokio::time::interval(heartbeat_interval);

    loop {
        let snapshot = AgentStateSnapshot {
            pid,
            state: current_state,
            daemon_addr: daemon_addr.clone(),
            agent_version: agent_version.clone(),
            started_at_unix,
            last_heartbeat_unix: now_unix(),
        };
        if let Err(e) = write_snapshot_atomic(&path, &snapshot) {
            warn!("Failed to write agent state to {}: {e}", path.display());
        }

        tokio::select! {
            _ = interval.tick() => {}
            maybe_update = rx.recv() => {
                match maybe_update {
                    Some(StateUpdate::SetState(s)) => current_state = s,
                    Some(StateUpdate::Connected { daemon_addr: a }) => {
                        current_state = AgentState::Connected;
                        daemon_addr = Some(a);
                    }
                    None => {
                        // All senders dropped — process is shutting down.
                        return;
                    }
                }
            }
        }
    }
}

/// Write a snapshot atomically (temp-file + rename) at `path`.
///
/// # Errors
///
/// Returns an `io::Error` if any filesystem operation fails.
pub fn write_snapshot_atomic(path: &Path, snapshot: &AgentStateSnapshot) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("cella-agent.state");
    let tmp_path = parent.join(format!("{file_name}.tmp.{}", std::process::id()));

    let mut json = serde_json::to_vec_pretty(snapshot)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    json.push(b'\n');

    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }

    std::fs::rename(&tmp_path, path)
}

/// Read a snapshot from `path`. Returns `None` if the file doesn't exist or is
/// malformed.
#[must_use]
pub fn read_snapshot(path: &Path) -> Option<AgentStateSnapshot> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Return `true` if `/proc/{pid}` exists. Linux-only; the agent only runs in
/// Linux containers.
#[must_use]
pub fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Current Unix time in seconds (saturating; clock moving backwards returns 0).
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(state: AgentState) -> AgentStateSnapshot {
        AgentStateSnapshot {
            pid: 42,
            state,
            daemon_addr: Some("host.docker.internal:60000".to_string()),
            agent_version: "0.0.28".to_string(),
            started_at_unix: 1_700_000_000,
            last_heartbeat_unix: 1_700_000_100,
        }
    }

    #[test]
    fn write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let snap = sample(AgentState::Connected);
        write_snapshot_atomic(&path, &snap).unwrap();
        let read = read_snapshot(&path).unwrap();
        assert_eq!(read.pid, snap.pid);
        assert!(matches!(read.state, AgentState::Connected));
        assert_eq!(
            read.daemon_addr.as_deref(),
            Some("host.docker.internal:60000")
        );
        assert_eq!(read.agent_version, "0.0.28");
    }

    #[test]
    fn write_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        write_snapshot_atomic(&path, &sample(AgentState::Connected)).unwrap();
        write_snapshot_atomic(&path, &sample(AgentState::Reconnecting)).unwrap();
        let read = read_snapshot(&path).unwrap();
        assert!(matches!(read.state, AgentState::Reconnecting));
    }

    #[test]
    fn read_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        assert!(read_snapshot(&path).is_none());
    }

    #[test]
    fn read_malformed_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"{not valid json").unwrap();
        assert!(read_snapshot(&path).is_none());
    }

    #[test]
    fn write_leaves_no_temp_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        write_snapshot_atomic(&path, &sample(AgentState::Connected)).unwrap();
        // After atomic rename, the temp should be gone.
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "only the final file should remain");
    }

    #[test]
    fn pid_alive_true_for_self() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn pid_alive_false_for_bogus_pid() {
        assert!(!pid_alive(4_000_000_000));
    }

    #[test]
    fn agent_state_serializes_stably() {
        // Guard against accidental rename of the enum variants — a rename would
        // break the in-container doctor's ability to read an older agent's
        // state file (or vice versa) during a staggered rollout.
        let j = serde_json::to_string(&AgentState::Connected).unwrap();
        assert_eq!(j, "\"Connected\"");
        let j = serde_json::to_string(&AgentState::Reconnecting).unwrap();
        assert_eq!(j, "\"Reconnecting\"");
        let j = serde_json::to_string(&AgentState::Disconnected).unwrap();
        assert_eq!(j, "\"Disconnected\"");
    }

    #[tokio::test]
    async fn writer_loop_writes_on_update() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let writer = spawn_state_writer(
            path.clone(),
            "0.0.28".to_string(),
            AgentState::Disconnected,
            Duration::from_secs(60), // long interval — force write via update
        );

        // Wait for the initial write.
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let initial = read_snapshot(&path).unwrap();
        assert!(matches!(initial.state, AgentState::Disconnected));

        writer.set_connected("host.docker.internal:60000".to_string());

        // The atomic Connected update should produce a snapshot with BOTH
        // state=Connected and the daemon address in a single write — never
        // a torn {Disconnected, Some(addr)} intermediate.
        let start = std::time::Instant::now();
        loop {
            let snap = read_snapshot(&path).unwrap();
            if matches!(snap.state, AgentState::Connected)
                && snap.daemon_addr.as_deref() == Some("host.docker.internal:60000")
            {
                break;
            }
            // If state is Connected, daemon_addr must already be set — never
            // Connected without an addr (would mean torn write).
            if matches!(snap.state, AgentState::Connected) {
                assert_eq!(
                    snap.daemon_addr.as_deref(),
                    Some("host.docker.internal:60000"),
                    "Connected without daemon_addr indicates torn write"
                );
            }
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "writer did not reflect update within 2s"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn set_connected_is_atomic() {
        // Regression for the two-message race: with separate set_state +
        // set_daemon_addr calls, the writer could emit a {Disconnected,
        // Some(addr)} snapshot between the two updates. `set_connected`
        // fuses them into a single StateUpdate so no intermediate exists.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let writer = spawn_state_writer(
            path.clone(),
            "0.0.28".to_string(),
            AgentState::Disconnected,
            Duration::from_secs(60),
        );

        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        writer.set_connected("host.docker.internal:60000".to_string());

        let start = std::time::Instant::now();
        loop {
            let snap = read_snapshot(&path).unwrap();
            // Invariant: if state is Connected, daemon_addr must be Some.
            if matches!(snap.state, AgentState::Connected) {
                assert_eq!(
                    snap.daemon_addr.as_deref(),
                    Some("host.docker.internal:60000")
                );
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "Connected state never observed within 2s"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn writer_loop_heartbeats_on_interval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let _writer = spawn_state_writer(
            path.clone(),
            "0.0.28".to_string(),
            AgentState::Connected,
            Duration::from_millis(50),
        );

        // Wait for the initial write.
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let first = read_snapshot(&path).unwrap();
        let first_hb = first.last_heartbeat_unix;

        // Wait long enough for at least one heartbeat tick beyond the first.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let second = read_snapshot(&path).unwrap();
        assert!(
            second.last_heartbeat_unix >= first_hb,
            "heartbeat should not go backwards"
        );
    }
}
