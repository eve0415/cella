use cella_env::{AGENT_LOG_PATH, AGENT_PATH};
use tracing::{info, warn};

use crate::traits::ContainerBackend;
use crate::types::{ExecOptions, ExecResult};

/// Environment variables injected when the managed cella-agent is available.
///
/// `wayland_clipboard` mirrors `settings.clipboard.wayland`. When on, the
/// container gets `WAYLAND_DISPLAY` pointing at the socket the agent daemon
/// serves, which is the only way clients that speak the Wayland protocol
/// directly — `arboard`, `wl-clipboard-rs` — can reach the host clipboard;
/// they never exec anything, so the `/cella/bin` shims are invisible to them.
///
/// `DISPLAY` is deliberately never set: it would activate `arboard`'s X11
/// backend, which has no server to talk to here.
///
/// `existing` is the env already destined for the container. These vars are
/// appended after it and Docker is last-wins on duplicate keys, so anything
/// the user set themselves would otherwise be silently overwritten —
/// `WAYLAND_DISPLAY` is therefore skipped when it is already present. A
/// devcontainer that bind-mounts a real compositor socket keeps working, and
/// the agent's own `should_serve` check then genuinely sees another
/// compositor's value rather than always seeing cella's.
pub fn agent_env_vars(wayland_clipboard: bool, existing: &[String]) -> Vec<String> {
    let version = env!("CARGO_PKG_VERSION");
    let mut vars = vec![
        "BROWSER=/cella/bin/cella-browser".to_string(),
        format!("CELLA_AGENT_VERSION={version}"),
    ];
    let user_set_display = existing.iter().any(|v| v.starts_with("WAYLAND_DISPLAY="));
    if wayland_clipboard && !user_set_display {
        vars.push(format!(
            "WAYLAND_DISPLAY={}",
            cella_protocol::WAYLAND_CLIPBOARD_SOCKET
        ));
    }
    vars
}

/// Shell fragment that points `$l` at [`AGENT_LOG_PATH`], or at `/dev/null` when that log cannot be opened, so an unopenable log never stops the daemon from starting.
///
/// The probe uses `true` rather than `:` because a redirection that fails on a special built-in makes `sh` exit.
pub fn open_agent_log() -> String {
    format!("l={AGENT_LOG_PATH}; true 2>/dev/null >>\"$l\" || l=/dev/null")
}

/// The no-op word that names the supervisor at the start of its cmdline.
const SUPERVISOR_MARKER: &str = "cella-agent-supervisor";

/// Shell loop that keeps `cella-agent daemon` running, respawning it one second after it exits.
///
/// The daemon's output is appended to [`AGENT_LOG_PATH`], which lives in root-owned `/run` so no other user can create it first.
/// When the log cannot be opened the loop logs to `/dev/null` (see [`open_agent_log`]), and it empties a log that has grown past 1 MiB before each respawn.
///
/// It is always launched as `sh -c '<loop>'`, so it must not contain a single quote.
/// The leading no-op `:` names the supervisor at the start of its cmdline (`sh -c : cella-agent-supervisor; …`), and the agent path is quoted so the unanchored `pkill -f 'cella-agent daemon'` of an older cella cannot match the supervisor.
fn supervisor_loop() -> String {
    format!(
        ": {SUPERVISOR_MARKER}; {}; \
         while true; do \
         [ \"$l\" = /dev/null ] || [ \"$(wc -c 2>/dev/null <\"$l\")\" -le 1048576 ] 2>/dev/null \
         || true 2>/dev/null >\"$l\"; \
         \"{AGENT_PATH}\" daemon \
         --poll-interval \"${{CELLA_PORT_POLL_INTERVAL:-1000}}\" >>\"$l\" 2>&1; \
         sleep 1; done",
        open_agent_log()
    )
}

/// Start of a running supervisor loop's cmdline (its argv joined by spaces).
fn supervisor_prefix() -> String {
    format!("sh -c : {SUPERVISOR_MARKER}")
}

/// Start of the agent daemon's cmdline; its argv has no quotes.
fn daemon_prefix() -> String {
    format!("{AGENT_PATH} daemon")
}

/// Shell command that starts the supervisor loop in the background, detached from the caller's stdin, stdout and stderr, so an exec that launches it ends as soon as its own script does.
///
/// That holds only while its `&` backgrounds this command alone, so it goes inside an `if` rather than at the end of a list such as `a || <spawn>`.
/// Such a list is backgrounded whole, and bash runs it in a subshell that keeps the caller's streams open while it waits for the supervisor.
pub fn spawn_agent_supervisor() -> String {
    format!("sh -c '{}' </dev/null >/dev/null 2>&1 &", supervisor_loop())
}

/// What the agent start script does to a daemon that is already running.
#[derive(Debug, Clone, Copy)]
enum RunningDaemon {
    /// Kill it, so a supervisor respawns it, and remove its proxy readiness marker.
    Kill,
    /// Leave it alone.
    Keep,
}

/// Script that starts a supervisor unless one is running, after killing a running daemon when asked to.
///
/// It scans `/proc` with only `tr`, `read` and `kill`, because many images ship no procps.
/// Each cmdline is matched by prefix against its NUL-separated argv joined by spaces.
/// The script's own argv, which its `$(…)` subshells share, begins `sh -c [ -x …`, so it can match neither prefix.
///
/// A process counts only when it shares the script's mount namespace, so that with `--pid=host` or a compose `pid:` another container's daemon is neither killed nor taken for this container's supervisor.
/// The namespace is checked with builtins before the cmdline is read, so another container's process costs no `tr` fork.
/// The namespace is told apart by the mount ID on the first line of `/proc/<pid>/mountinfo`, its root mount: unlike `/proc/<pid>/ns/mnt`, that file is readable without `CAP_SYS_PTRACE` for a process of another user, such as the non-root daemon of a container created by an older cella.
///
/// The restart removes the readiness marker after killing the daemon and before any supervisor is started, so the marker a wait sees next is the respawned daemon's.
fn agent_script(daemon: RunningDaemon) -> String {
    let (kill_daemon, clear_marker) = match daemon {
        RunningDaemon::Kill => (
            format!("\n    \"{}\"*) kill \"$p\" 2>/dev/null ;;", daemon_prefix()),
            format!("rm -f \"{}\"\n", cella_env::AGENT_PROXY_READY_PATH),
        ),
        RunningDaemon::Keep => (String::new(), String::new()),
    };
    let supervisor_prefix = supervisor_prefix();
    format!(
        "[ -x {AGENT_PATH} ] || exit 0\n\
         read -r m _ </proc/$$/mountinfo\n\
         same() {{ read -r i _ 2>/dev/null <\"$d/mountinfo\" && [ \"$i\" = \"$m\" ]; }}\n\
         sup=\n\
         for d in /proc/[0-9]*; do\n  \
         p=${{d#/proc/}}\n  \
         [ \"$p\" = \"$$\" ] && continue\n  \
         same || continue\n  \
         c=$(tr '\\0' ' ' 2>/dev/null < \"$d/cmdline\") || continue\n  \
         case \"$c\" in{kill_daemon}\n    \
         \"{supervisor_prefix}\"*) sup=1 ;;\n  \
         esac\n\
         done\n\
         {clear_marker}\
         {}",
        spawn_unless_supervised(&spawn_agent_supervisor())
    )
}

/// The last line of [`agent_script`]: run `spawn`, a command ending in `&`, unless the scan found a supervisor.
fn spawn_unless_supervised(spawn: &str) -> String {
    format!("if [ -z \"$sup\" ]; then {spawn} fi")
}

/// Run [`agent_script`] as root and wait for it to exit.
///
/// The script's `&` backgrounds only the supervisor command, whose streams are redirected, so nothing left running holds the exec's streams and the exec ends when the script does rather than when the supervisor would, whether `sh` is dash or bash.
async fn run_agent_script(
    client: &dyn ContainerBackend,
    container_id: &str,
    daemon: RunningDaemon,
) -> Result<ExecResult, crate::BackendError> {
    client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["sh".to_string(), "-c".to_string(), agent_script(daemon)],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
}

/// Log the outcome of [`run_agent_script`] under `action`.
fn report_agent_script(
    action: &str,
    container_id: &str,
    result: Result<ExecResult, crate::BackendError>,
) {
    match result {
        Ok(r) if r.exit_code == 0 => info!("Agent {action} done in container {container_id}"),
        Ok(r) => warn!(
            "Agent {action} script exited with code {} in container {container_id}: {}",
            r.exit_code,
            r.stderr.trim()
        ),
        Err(e) => warn!("Failed to run the agent {action} script in container {container_id}: {e}"),
    }
}

/// Restart the agent daemon as root.
///
/// A running supervisor respawns the killed daemon within a second.
/// A container without one (a non-root entrypoint, compose, `overrideCommand: false`, or a container created before the supervisor existed) gets a root supervisor started here.
/// A container without the agent binary is left alone.
///
/// Returns once the script has exited, by which point the killed daemon's proxy readiness marker is gone, so a wait for the marker that follows sees only the respawned daemon's.
pub async fn restart_agent_in_container(client: &dyn ContainerBackend, container_id: &str) {
    let result = run_agent_script(client, container_id, RunningDaemon::Kill).await;
    report_agent_script("restart", container_id, result);
}

/// Make sure the agent runs, as root, without restarting a running daemon.
///
/// Starts a root supervisor only when none is running, like [`restart_agent_in_container`] but leaving a running daemon and its readiness marker alone.
pub async fn ensure_agent_running(client: &dyn ContainerBackend, container_id: &str) {
    let result = run_agent_script(client, container_id, RunningDaemon::Keep).await;
    report_agent_script("start", container_id, result);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a process with `argv` matches `prefix` the way
    /// [`agent_script`] matches it: its argv joined by spaces starts with it.
    fn cmdline_starts_with(prefix: &str, argv: &[&str]) -> bool {
        argv.join(" ").starts_with(prefix)
    }

    const DAEMON_ARGV: &[&str] = &[AGENT_PATH, "daemon", "--poll-interval", "1000"];
    const MODES: [RunningDaemon; 2] = [RunningDaemon::Kill, RunningDaemon::Keep];

    /// Whether a supervisor loop's argv (`sh -c <loop>`) is matched by `matches`.
    fn supervisor_matches(matches: impl Fn(&[&str]) -> bool) -> bool {
        matches(&["sh", "-c", &supervisor_loop()])
    }

    #[test]
    fn prefixes_do_not_match_the_script_itself() {
        for mode in MODES {
            let script = agent_script(mode);
            let script_argv = ["sh", "-c", &script];
            for prefix in [daemon_prefix(), supervisor_prefix()] {
                assert!(
                    !cmdline_starts_with(&prefix, &script_argv),
                    "{prefix} matches the {mode:?} script's own argv"
                );
            }
        }
    }

    #[test]
    fn only_the_restart_script_kills_the_daemon() {
        assert!(agent_script(RunningDaemon::Kill).contains(&daemon_prefix()));
        assert!(!agent_script(RunningDaemon::Keep).contains(&daemon_prefix()));
        for mode in MODES {
            assert!(agent_script(mode).contains(&supervisor_prefix()));
        }
    }

    #[test]
    fn scripts_parse() {
        for mode in MODES {
            let script = agent_script(mode);
            let status = std::process::Command::new("sh")
                .args(["-n", "-c", &script])
                .status()
                .unwrap();
            assert!(
                status.success(),
                "sh rejected the {mode:?} script:\n{script}"
            );
        }
    }

    #[test]
    fn daemon_prefix_matches_only_the_daemon() {
        let prefix = daemon_prefix();
        assert!(cmdline_starts_with(&prefix, DAEMON_ARGV));
        assert!(!supervisor_matches(|argv| cmdline_starts_with(
            &prefix, argv
        )));
    }

    #[test]
    fn supervisor_prefix_matches_only_the_supervisor() {
        let prefix = supervisor_prefix();
        assert!(supervisor_matches(|argv| cmdline_starts_with(
            &prefix, argv
        )));
        assert!(!cmdline_starts_with(&prefix, DAEMON_ARGV));
    }

    /// Containers created by an older cella still run its unanchored
    /// `pkill -f 'cella-agent daemon'`, which matches anywhere in the
    /// cmdline; it must not kill the new supervisor.
    #[test]
    fn older_unanchored_pkill_spares_the_supervisor() {
        let matches = |argv: &[&str]| argv.join(" ").contains("cella-agent daemon");
        assert!(matches(DAEMON_ARGV));
        assert!(!supervisor_matches(matches));
    }

    #[test]
    fn supervisor_loop_fits_in_single_quotes() {
        assert!(!supervisor_loop().contains('\''));
    }

    /// With `--pid=host` or a compose `pid:`, `/proc` lists other containers' processes.
    /// The scan skips every process that fails the script's own `same` guard before reading its cmdline, so no arm sees one; the guard is run here against the script's own process and against a process directory whose root mount differs.
    #[test]
    fn scripts_act_only_on_processes_in_their_own_mount_namespace() {
        let foreign = tempfile::tempdir().unwrap();
        std::fs::write(
            foreign.path().join("mountinfo"),
            "4294967295 1 0:1 / / rw - tmpfs tmpfs rw\n",
        )
        .unwrap();
        for mode in MODES {
            let script = agent_script(mode);
            // Everything between the binary check and the scan: the guard's setup.
            let guard = script
                .lines()
                .skip(1)
                .take_while(|l| !l.starts_with("for "))
                .collect::<Vec<_>>()
                .join("\n");
            let same = |d: &str| {
                std::process::Command::new("sh")
                    .args(["-c", &format!("{guard}\nd=\"{d}\"\nsame")])
                    .status()
                    .unwrap()
                    .success()
            };
            assert!(same("/proc/$$"), "{mode:?}: own process refused:\n{guard}");
            assert!(
                !same(&foreign.path().display().to_string()),
                "{mode:?}: a different root mount accepted:\n{guard}"
            );
            let lines: Vec<&str> = script.lines().map(str::trim).collect();
            let scan = lines.iter().position(|l| l.starts_with("for "));
            let skip = lines.iter().position(|l| *l == "same || continue");
            let read = lines.iter().position(|l| l.starts_with("c=$("));
            assert!(
                matches!((scan, skip, read), (Some(s), Some(g), Some(r)) if s < g && g < r),
                "{mode:?}: the scan does not skip foreign processes before reading their cmdline:\n{script}"
            );
        }
    }

    /// A marker removed before the kill could be rewritten by the dying daemon, and one removed after the spawn could be the fresh daemon's.
    #[test]
    fn only_the_restart_clears_the_readiness_marker_between_kill_and_spawn() {
        let clear = format!("rm -f \"{}\"", cella_env::AGENT_PROXY_READY_PATH);
        let script = agent_script(RunningDaemon::Kill);
        let kill = script.find("kill \"$p\"").expect("kill");
        let loop_end = script.find("\ndone\n").expect("loop end");
        let cleared = script.find(&clear).expect("marker cleared");
        let spawn = script.find(&spawn_agent_supervisor()).expect("spawn");
        assert!(
            kill < loop_end && loop_end < cleared && cleared < spawn,
            "{script}"
        );
        assert!(!agent_script(RunningDaemon::Keep).contains(cella_env::AGENT_PROXY_READY_PATH));
    }

    #[test]
    fn scripts_end_by_spawning_the_supervisor_unless_one_runs() {
        let tail = spawn_unless_supervised(&spawn_agent_supervisor());
        for mode in MODES {
            assert!(agent_script(mode).ends_with(&tail), "{mode:?}");
        }
    }

    /// Run `script` under `shell` with its output piped, as an exec runs it, and return how long the shell held the pipes.
    /// `None` when the shell is not installed.
    fn time_until_streams_close(shell: &[&str], script: &str) -> Option<std::time::Duration> {
        let begun = std::time::Instant::now();
        let output = match std::process::Command::new(shell[0])
            .args(&shell[1..])
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
        {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            output => output.unwrap(),
        };
        let held = begun.elapsed();
        assert!(output.status.success(), "{shell:?}: {output:?}");
        Some(held)
    }

    /// Regression: `[ -n "$sup" ] || <spawn> &` backgrounds the whole list, and bash (the `sh` of Fedora and RHEL-family images) runs it in a subshell that keeps the exec's streams open until the spawned process exits, so the exec never returned while the supervisor ran.
    /// The spawn is [`spawn_agent_supervisor`]'s own command with only the supervisor loop swapped for one that marks that it ran and then lingers.
    #[test]
    fn the_spawn_does_not_hold_the_streams_under_dash_or_bash() {
        let dir = tempfile::tempdir().unwrap();
        let shells: [&[&str]; 2] = [&["sh", "-c"], &["bash", "--posix", "-c"]];
        let real_spawn = spawn_agent_supervisor();
        for (i, shell) in shells.into_iter().enumerate() {
            let spawned = dir.path().join(format!("spawned-{i}"));
            let unwanted = dir.path().join(format!("unwanted-{i}"));
            let spawn = |marker: &std::path::Path| {
                let stand_in = format!("touch \"{}\"; sleep 5", marker.display());
                let spawn = real_spawn.replace(&supervisor_loop(), &stand_in);
                assert_ne!(spawn, real_spawn, "the loop was not swapped out");
                spawn
            };
            let supervised = format!("sup=1\n{}", spawn_unless_supervised(&spawn(&unwanted)));
            let unsupervised = format!("sup=\n{}", spawn_unless_supervised(&spawn(&spawned)));

            let Some(_) = time_until_streams_close(shell, &supervised) else {
                eprintln!("skipping {shell:?}: not installed");
                continue;
            };
            let held = time_until_streams_close(shell, &unsupervised).unwrap();

            assert!(
                held < std::time::Duration::from_secs(2),
                "{shell:?} held the streams for {held:?}"
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !spawned.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(
                spawned.exists(),
                "{shell:?} did not spawn without a supervisor"
            );
            assert!(!unwanted.exists(), "{shell:?} spawned despite a supervisor");
        }
    }

    #[test]
    fn agent_env_vars_sets_wayland_display_when_enabled() {
        let vars = agent_env_vars(true, &[]);
        assert!(
            vars.iter().any(|v| v
                == &format!(
                    "WAYLAND_DISPLAY={}",
                    cella_protocol::WAYLAND_CLIPBOARD_SOCKET
                )),
            "got {vars:?}"
        );
    }

    #[test]
    fn agent_env_vars_omits_wayland_display_when_disabled() {
        assert!(
            !agent_env_vars(false, &[])
                .iter()
                .any(|v| v.starts_with("WAYLAND_DISPLAY="))
        );
    }

    #[test]
    fn agent_env_vars_never_sets_display() {
        // Setting DISPLAY would send arboard down its X11 path and reintroduce
        // the "X11 server connection timed out" failure this work removes.
        for enabled in [true, false] {
            assert!(
                !agent_env_vars(enabled, &[])
                    .iter()
                    .any(|v| v.starts_with("DISPLAY=")),
                "DISPLAY must never be set (wayland_clipboard={enabled})"
            );
        }
    }

    /// Regression: the agent vars are appended last and Docker is last-wins,
    /// so injecting unconditionally silently replaced a `containerEnv` /
    /// `remoteEnv` `WAYLAND_DISPLAY` pointing at a real compositor — GUI apps
    /// in that container would then find cella's clipboard-only endpoint and
    /// exit without a window.
    #[test]
    fn agent_env_vars_yields_to_a_user_supplied_wayland_display() {
        let existing = vec!["WAYLAND_DISPLAY=wayland-0".to_string()];
        assert!(
            !agent_env_vars(true, &existing)
                .iter()
                .any(|v| v.starts_with("WAYLAND_DISPLAY=")),
            "must not override a compositor the user configured"
        );
    }

    #[test]
    fn agent_env_vars_contains_browser() {
        let vars = agent_env_vars(true, &[]);
        let browser = vars.iter().find(|v| v.starts_with("BROWSER="));
        assert!(browser.is_some(), "BROWSER env var must be present");
        assert!(
            browser.unwrap().contains("/cella/bin/cella-browser"),
            "BROWSER should point to the managed browser helper"
        );
    }

    #[test]
    fn agent_env_vars_contains_version() {
        let vars = agent_env_vars(true, &[]);
        let version = vars.iter().find(|v| v.starts_with("CELLA_AGENT_VERSION="));
        assert!(
            version.is_some(),
            "CELLA_AGENT_VERSION env var must be present"
        );
    }
}
