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
        Some(Self(()))
    }
}

/// Convenience: build a guard from a resolved container. Pulls `branch` from
/// the `dev.cella.branch` label when present, and `service` from the caller's
/// explicit flag.
pub fn push_for_container(
    container: &cella_backend::ContainerInfo,
    service: Option<&str>,
    subcommand: &'static str,
) -> Option<TitleGuard> {
    TitleGuard::push(&TitleContent {
        name: title_name(&container.name).to_string(),
        service: service.map(str::to_string),
        branch: container.labels.get("dev.cella.branch").cloned(),
        subcommand,
    })
}

/// Convenience: build a guard from a container name alone. Used by commands
/// that know the deterministic container name (via [`cella_backend::container_name`]
/// or `UpContext::container_nm`) without having a full [`cella_backend::ContainerInfo`]
/// to pull labels from.
pub fn push_for_name(
    container_name: &str,
    branch: Option<&str>,
    subcommand: &'static str,
) -> Option<TitleGuard> {
    TitleGuard::push(&TitleContent {
        name: title_name(container_name).to_string(),
        service: None,
        branch: branch.map(str::to_string),
        subcommand,
    })
}

impl Drop for TitleGuard {
    fn drop(&mut self) {
        let tmux = in_tmux();
        let mut stderr = io::stderr().lock();
        let _ = emit_pop(&mut stderr, tmux);
        let _ = stderr.flush();
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
