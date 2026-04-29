//! Terminal title integration.
//!
//! Emits OSC escape sequences to set the terminal window title for the duration
//! of interactive or long-running `cella` commands, and restores the prior
//! title on exit using the xterm push/pop title stack (OSC 22 / 23).
//!
//! See the design doc at `docs/…` — or, more practically, the tests below for
//! the exact byte-level behavior.
//!
//! Title shape: `<name>[:service][@branch] \u{2014} cella <subcommand>`.
//!
//! The emitted bytes go to `stderr`, and emission is gated on `stderr` being a
//! TTY. When cella is run inside `tmux` (detected via `$TMUX`), every escape
//! sequence is wrapped in a DCS passthrough so the outer terminal's title
//! actually updates.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

static TITLE_ACTIVE: AtomicBool = AtomicBool::new(false);
static NEEDS_TMUX_WRAP: AtomicBool = AtomicBool::new(false);

const RESTORE_PLAIN: &[u8] = b"\x1b[23;0t";
const RESTORE_TMUX: &[u8] = b"\x1bPtmux;\x1b\x1b[23;0t\x1b\\";

/// Strip the `"cella-"` prefix so the title doesn't repeat the brand already
/// present in the `" \u{2014} cella <subcommand>"` suffix. No-op if the prefix
/// is absent.
pub fn title_name(container_name: &str) -> &str {
    container_name
        .strip_prefix("cella-")
        .unwrap_or(container_name)
}

/// Inputs to title rendering. Constructed by call sites from the resolved
/// container name, labels, and CLI args.
pub struct TitleContent {
    /// Container short-name with `cella-` prefix already stripped (use
    /// [`title_name`]).
    pub name: String,
    /// Compose service, only populated when the user passed `--service <x>`.
    pub service: Option<String>,
    /// Branch label, only populated when the container carries the
    /// `dev.cella.branch` label (i.e. a worktree container).
    pub branch: Option<String>,
    /// Clap subcommand literal — `"shell"`, `"up"`, etc. Never includes
    /// arguments.
    pub subcommand: &'static str,
}

impl TitleContent {
    /// Render to the final title string (without any escape bytes).
    pub fn format(&self) -> String {
        let mut s = self.name.clone();
        if let Some(svc) = &self.service {
            s.push(':');
            s.push_str(svc);
        }
        if let Some(br) = &self.branch {
            s.push('@');
            s.push_str(br);
        }
        s.push_str(" \u{2014} cella ");
        s.push_str(self.subcommand);
        s
    }
}

/// RAII guard that sets the terminal title on construction and pops it on
/// drop. Returns `None` when `stderr` isn't a TTY, so the common pattern
///
/// ```ignore
/// let _guard = TitleGuard::push(&content);
/// ```
///
/// is always safe regardless of environment.
///
/// **Note on `std::process::exit`:** the stdlib `exit` function skips Drop.
/// Commands that call `process::exit` must `drop(guard)` explicitly before the
/// exit call, mirroring the `RawModeGuard` pattern in `cella-docker`.
#[must_use = "binding required to keep title set until the guard is dropped"]
pub struct TitleGuard(());

impl TitleGuard {
    /// Emit OSC 22 (push) + OSC 0 (set). Skips emission entirely when
    /// `stderr` is not a TTY.
    pub fn push(content: &TitleContent) -> Option<Self> {
        if !io::stderr().is_terminal() {
            return None;
        }
        let tmux = in_tmux();
        let mut stderr = io::stderr().lock();
        // Best-effort: if a write fails (e.g. the terminal vanished),
        // we still want to hand out the guard so Drop's pop is attempted.
        let _ = emit_push(&mut stderr, tmux);
        let _ = emit_set(&mut stderr, &content.format(), tmux);
        let _ = stderr.flush();
        NEEDS_TMUX_WRAP.store(tmux, Ordering::Release);
        TITLE_ACTIVE.store(true, Ordering::Release);
        Some(Self(()))
    }
}

/// Convenience: build a guard from a resolved container. Pulls `branch` from
/// the `dev.cella.branch` label when present, and `service` from the caller's
/// explicit flag. For compose containers, prefers the `com.docker.compose.project`
/// label as the name source so the title reads as the project (not
/// `<project>-<service>-<index>` which would duplicate the service suffix).
pub fn push_for_container(
    container: &cella_backend::ContainerInfo,
    service: Option<&str>,
    subcommand: &'static str,
) -> Option<TitleGuard> {
    TitleGuard::push(&TitleContent {
        name: title_name(base_name(container)).to_string(),
        service: service.map(str::to_string),
        branch: container.labels.get("dev.cella.branch").cloned(),
        subcommand,
    })
}

/// Look up the existing container for `workspace_root` and derive a guard from
/// its labels (compose project, branch). Falls back to `fallback_name` when no
/// container exists yet.
///
/// `branch` is the caller's authoritative branch (e.g. from `--branch <b>` or
/// from `cella branch <name>`). When provided, it always wins over whatever
/// `dev.cella.branch` label the live container happens to carry — a fresh
/// `cella branch` creation has no container label yet, and a retry after a
/// partial failure may see a stale or mismatched one. `None` means the caller
/// doesn't know the branch a priori, so the container label (if any) is used.
pub async fn push_for_workspace(
    client: &dyn cella_backend::ContainerBackend,
    workspace_root: &std::path::Path,
    fallback_name: &str,
    service: Option<&str>,
    branch: Option<&str>,
    subcommand: &'static str,
) -> Option<TitleGuard> {
    let container = client.find_container(workspace_root).await.ok().flatten();
    let name = title_name(container.as_ref().map_or(fallback_name, base_name)).to_string();
    let effective_branch = branch.map(String::from).or_else(|| {
        container
            .as_ref()
            .and_then(|c| c.labels.get("dev.cella.branch").cloned())
    });
    TitleGuard::push(&TitleContent {
        name,
        service: service.map(str::to_string),
        branch: effective_branch,
        subcommand,
    })
}

/// Prefer the compose project label so that compose containers surface as
/// `<project>` (e.g. `cella-myrepo-a1b2c3d4`) instead of
/// `<project>-<service>-<index>` (e.g. `cella-myrepo-a1b2c3d4-api-1`).
fn base_name(container: &cella_backend::ContainerInfo) -> &str {
    container
        .labels
        .get("com.docker.compose.project")
        .map_or(container.name.as_str(), String::as_str)
}

impl Drop for TitleGuard {
    fn drop(&mut self) {
        let tmux = in_tmux();
        let mut stderr = io::stderr().lock();
        let _ = emit_restore(&mut stderr, tmux);
        let _ = stderr.flush();
        // Clear after writing so a signal during Drop still triggers the handler.
        // Worst case both paths emit — a harmless double-pop that the shell prompt resets.
        TITLE_ACTIVE.store(false, Ordering::Release);
    }
}

fn in_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
}

fn emit_push(w: &mut impl Write, in_tmux: bool) -> io::Result<()> {
    // CSI 22 ; 0 t — save both icon name and window title to the terminal's stack.
    let bytes: &[u8] = b"\x1b[22;0t";
    if in_tmux {
        w.write_all(&tmux_wrap(bytes))
    } else {
        w.write_all(bytes)
    }
}

fn emit_set(w: &mut impl Write, title: &str, in_tmux: bool) -> io::Result<()> {
    // OSC 0 — set both icon name and window title. BEL terminator is preferred
    // over ST because it doesn't collide with the DCS terminator when wrapped.
    let mut bytes = b"\x1b]0;".to_vec();
    bytes.extend_from_slice(title.as_bytes());
    bytes.push(0x07);
    if in_tmux {
        w.write_all(&tmux_wrap(&bytes))
    } else {
        w.write_all(&bytes)
    }
}

// Pop the title stack. Terminals with stack support (xterm, kitty, iTerm2)
// restore the prior title; terminals without (WezTerm) silently ignore the
// pop and leave the cella title until the shell prompt overwrites it.
fn emit_restore(w: &mut impl Write, in_tmux: bool) -> io::Result<()> {
    emit_pop(w, in_tmux)
}

fn emit_pop(w: &mut impl Write, in_tmux: bool) -> io::Result<()> {
    // CSI 23 ; 0 t — restore icon name and window title from the stack.
    let bytes: &[u8] = b"\x1b[23;0t";
    if in_tmux {
        w.write_all(&tmux_wrap(bytes))
    } else {
        w.write_all(bytes)
    }
}

/// Wrap an escape sequence in a tmux DCS passthrough envelope so the outer
/// terminal receives the inner sequence. Each ESC inside is doubled per tmux
/// convention.
fn tmux_wrap(inner: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(inner.len() + 10);
    out.extend_from_slice(b"\x1bPtmux;");
    for &b in inner {
        if b == 0x1b {
            out.push(0x1b);
        }
        out.push(b);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// Install SIGINT/SIGTERM handlers that restore the terminal title before the
/// process exits. Call once, early in `main`, before any `TitleGuard` is created.
///
/// Uses `SA_RESETHAND` so the handler fires at most once, then re-raises the
/// signal with the default disposition — preserving exit status 128+signum for
/// parent processes.
pub fn install_signal_handlers() {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};

    let action = SigAction::new(
        SigHandler::Handler(restore_title_on_signal),
        SaFlags::SA_RESETHAND,
        SigSet::empty(),
    );
    #[allow(unsafe_code)]
    // SAFETY: sigaction is async-signal-safe; the handler only uses
    // async-signal-safe operations (atomic loads + libc::write + libc::raise).
    unsafe {
        let _ = sigaction(Signal::SIGINT, &action);
        let _ = sigaction(Signal::SIGTERM, &action);
    }
}

extern "C" fn restore_title_on_signal(sig: nix::libc::c_int) {
    if TITLE_ACTIVE.load(Ordering::Acquire) {
        let bytes = if NEEDS_TMUX_WRAP.load(Ordering::Acquire) {
            RESTORE_TMUX
        } else {
            RESTORE_PLAIN
        };
        #[allow(unsafe_code)]
        // SAFETY: libc::write on STDERR_FILENO is async-signal-safe.
        unsafe {
            let _ = nix::libc::write(nix::libc::STDERR_FILENO, bytes.as_ptr().cast(), bytes.len());
        }
    }
    #[allow(unsafe_code)]
    // SAFETY: raise is async-signal-safe. SA_RESETHAND already restored the
    // default disposition, so this re-delivers the signal with SIG_DFL.
    unsafe {
        nix::libc::raise(sig);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── title_name ──────────────────────────────────────────────────

    #[test]
    fn title_name_strips_prefix() {
        assert_eq!(title_name("cella-foo-abcd1234"), "foo-abcd1234");
    }

    #[test]
    fn title_name_no_prefix_unchanged() {
        assert_eq!(title_name("other"), "other");
    }

    #[test]
    fn title_name_empty_string() {
        assert_eq!(title_name(""), "");
    }

    #[test]
    fn title_name_just_prefix() {
        assert_eq!(title_name("cella-"), "");
    }

    #[test]
    fn title_name_prefix_without_trailing() {
        // "cella" without the trailing dash is NOT a match — strip is literal.
        assert_eq!(title_name("cella"), "cella");
    }

    // ── TitleContent::format ────────────────────────────────────────

    #[test]
    fn format_name_only() {
        let c = TitleContent {
            name: "x".to_string(),
            service: None,
            branch: None,
            subcommand: "shell",
        };
        assert_eq!(c.format(), "x \u{2014} cella shell");
    }

    #[test]
    fn format_with_service() {
        let c = TitleContent {
            name: "x".to_string(),
            service: Some("api".to_string()),
            branch: None,
            subcommand: "shell",
        };
        assert_eq!(c.format(), "x:api \u{2014} cella shell");
    }

    #[test]
    fn format_with_branch() {
        let c = TitleContent {
            name: "x".to_string(),
            service: None,
            branch: Some("feat/auth".to_string()),
            subcommand: "shell",
        };
        assert_eq!(c.format(), "x@feat/auth \u{2014} cella shell");
    }

    #[test]
    fn format_all_three() {
        let c = TitleContent {
            name: "myrepo-a1b2c3d4".to_string(),
            service: Some("api".to_string()),
            branch: Some("feat/auth".to_string()),
            subcommand: "up",
        };
        assert_eq!(
            c.format(),
            "myrepo-a1b2c3d4:api@feat/auth \u{2014} cella up"
        );
    }

    // ── emit_* plain ────────────────────────────────────────────────

    #[test]
    fn emit_push_plain_bytes() {
        let mut out = Vec::new();
        emit_push(&mut out, false).unwrap();
        assert_eq!(out, b"\x1b[22;0t");
    }

    #[test]
    fn emit_set_plain_bytes() {
        let mut out = Vec::new();
        emit_set(&mut out, "x \u{2014} cella shell", false).unwrap();
        // Em dash is UTF-8 0xE2 0x80 0x94.
        assert_eq!(out, b"\x1b]0;x \xe2\x80\x94 cella shell\x07");
    }

    #[test]
    fn emit_pop_plain_bytes() {
        let mut out = Vec::new();
        emit_pop(&mut out, false).unwrap();
        assert_eq!(out, b"\x1b[23;0t");
    }

    // ── emit_* tmux-wrapped ─────────────────────────────────────────

    #[test]
    fn emit_push_tmux_bytes() {
        let mut out = Vec::new();
        emit_push(&mut out, true).unwrap();
        // DCS envelope with doubled ESC inside.
        assert_eq!(out, b"\x1bPtmux;\x1b\x1b[22;0t\x1b\\");
    }

    #[test]
    fn emit_set_tmux_bytes() {
        let mut out = Vec::new();
        emit_set(&mut out, "x", true).unwrap();
        // Inner OSC \x1b]0;x\x07 becomes \x1b\x1b]0;x\x07 inside the envelope.
        assert_eq!(out, b"\x1bPtmux;\x1b\x1b]0;x\x07\x1b\\");
    }

    #[test]
    fn emit_pop_tmux_bytes() {
        let mut out = Vec::new();
        emit_pop(&mut out, true).unwrap();
        assert_eq!(out, b"\x1bPtmux;\x1b\x1b[23;0t\x1b\\");
    }

    // ── tmux_wrap ───────────────────────────────────────────────────

    #[test]
    fn tmux_wrap_no_escapes_in_inner() {
        assert_eq!(tmux_wrap(b"hello"), b"\x1bPtmux;hello\x1b\\");
    }

    #[test]
    fn tmux_wrap_single_escape_is_doubled() {
        assert_eq!(tmux_wrap(b"\x1b[X"), b"\x1bPtmux;\x1b\x1b[X\x1b\\");
    }

    #[test]
    fn tmux_wrap_multiple_escapes_each_doubled() {
        assert_eq!(
            tmux_wrap(b"\x1bA\x1bB"),
            b"\x1bPtmux;\x1b\x1bA\x1b\x1bB\x1b\\"
        );
    }

    // ── base_name selection (compose project vs container name) ─────

    fn container_with_labels(name: &str, labels: &[(&str, &str)]) -> cella_backend::ContainerInfo {
        let labels: std::collections::HashMap<String, String> = labels
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        cella_backend::ContainerInfo {
            id: "id".to_string(),
            name: name.to_string(),
            state: cella_backend::ContainerState::Running,
            exit_code: None,
            labels,
            config_hash: None,
            ports: vec![],
            created_at: None,
            container_user: None,
            image: None,
            mounts: vec![],
            backend: cella_backend::BackendKind::Docker,
        }
    }

    #[test]
    fn base_name_prefers_compose_project_label() {
        // Compose container: actual name has -<service>-<index> suffix, but
        // `com.docker.compose.project` points at the clean project name.
        let c = container_with_labels(
            "cella-myrepo-a1b2c3d4-api-1",
            &[("com.docker.compose.project", "cella-myrepo-a1b2c3d4")],
        );
        assert_eq!(base_name(&c), "cella-myrepo-a1b2c3d4");
    }

    #[test]
    fn base_name_falls_back_to_container_name_when_no_compose_label() {
        let c = container_with_labels("cella-myrepo-a1b2c3d4", &[]);
        assert_eq!(base_name(&c), "cella-myrepo-a1b2c3d4");
    }

    #[test]
    fn push_for_container_compose_title_has_no_redundant_service_suffix() {
        // Compose + explicit --service api: title should show project + :api,
        // NOT project-api-1:api. (Regression: codex stop-time review caught
        // this.)
        let c = container_with_labels(
            "cella-myrepo-a1b2c3d4-api-1",
            &[
                ("com.docker.compose.project", "cella-myrepo-a1b2c3d4"),
                ("com.docker.compose.service", "api"),
            ],
        );
        // Format directly (no OSC emission under non-TTY) to verify composition.
        let content = TitleContent {
            name: title_name(base_name(&c)).to_string(),
            service: Some("api".to_string()),
            branch: None,
            subcommand: "shell",
        };
        assert_eq!(content.format(), "myrepo-a1b2c3d4:api \u{2014} cella shell");
    }

    #[test]
    fn push_for_container_worktree_surfaces_branch_label() {
        // Container exists with dev.cella.branch label set by `cella branch`.
        let c = container_with_labels(
            "cella-myrepo-deadbe12",
            &[("dev.cella.branch", "feat/auth")],
        );
        let content = TitleContent {
            name: title_name(base_name(&c)).to_string(),
            service: None,
            branch: c.labels.get("dev.cella.branch").cloned(),
            subcommand: "up",
        };
        assert_eq!(
            content.format(),
            "myrepo-deadbe12@feat/auth \u{2014} cella up"
        );
    }

    /// Mirrors the branch-merge logic inside `push_for_workspace` so it can be
    /// unit-tested without a mock `ContainerBackend`.
    fn effective_branch(explicit: Option<&str>, container_label: Option<&str>) -> Option<String> {
        explicit
            .map(String::from)
            .or_else(|| container_label.map(String::from))
    }

    #[test]
    fn explicit_branch_wins_over_container_label() {
        // `cella branch feat/new` with a stale leftover container labelled
        // `feat/old` must title as feat/new (the user's intent), not feat/old.
        assert_eq!(
            effective_branch(Some("feat/new"), Some("feat/old")),
            Some("feat/new".to_string())
        );
    }

    #[test]
    fn explicit_branch_used_when_no_container() {
        // `cella branch feat/auth` on a fresh worktree (no container yet).
        assert_eq!(
            effective_branch(Some("feat/auth"), None),
            Some("feat/auth".to_string())
        );
    }

    #[test]
    fn container_label_used_when_no_explicit_branch() {
        // `cella up` (no --branch) in an existing worktree container.
        assert_eq!(
            effective_branch(None, Some("feat/auth")),
            Some("feat/auth".to_string())
        );
    }

    #[test]
    fn no_branch_without_explicit_or_label() {
        // Plain `cella up` in a non-worktree with no existing container.
        assert_eq!(effective_branch(None, None), None);
    }

    // ── emit_restore ─────────────────────────────────────────────────

    #[test]
    fn emit_restore_plain_emits_pop_only() {
        let mut out = Vec::new();
        emit_restore(&mut out, false).unwrap();
        assert_eq!(out, b"\x1b[23;0t");
    }

    #[test]
    fn emit_restore_tmux_emits_wrapped_pop_only() {
        let mut out = Vec::new();
        emit_restore(&mut out, true).unwrap();
        assert_eq!(out, tmux_wrap(b"\x1b[23;0t"));
    }

    // ── signal handler constants ───────────────────────────────────────

    #[test]
    fn restore_plain_matches_emit_restore_output() {
        let mut out = Vec::new();
        emit_restore(&mut out, false).unwrap();
        assert_eq!(out.as_slice(), RESTORE_PLAIN);
    }

    #[test]
    fn restore_tmux_matches_emit_restore_output() {
        let mut out = Vec::new();
        emit_restore(&mut out, true).unwrap();
        assert_eq!(out.as_slice(), RESTORE_TMUX);
    }

    // ── public API reachability ──────────────────────────────────────

    #[test]
    fn title_guard_push_is_reachable() {
        // Smoke test: exercises the public `push` + Drop path. In headless
        // test environments stderr is not a TTY so `push` returns None and
        // no bytes are emitted. In an interactive `cargo test`, it returns
        // `Some` and the guard is dropped immediately, popping the title.
        // Either outcome is fine; this test only asserts the call is sound.
        let content = TitleContent {
            name: "t".to_string(),
            service: None,
            branch: None,
            subcommand: "test",
        };
        drop(TitleGuard::push(&content));
    }
}
