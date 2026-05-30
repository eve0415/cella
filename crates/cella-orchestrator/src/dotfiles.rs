//! In-container dotfiles installation.
//!
//! Clones a dotfiles repository into the container as the remote user and runs
//! its install command, mirroring the official devcontainer CLI behavior in
//! `src/spec-common/dotfiles.ts`. The whole install runs in a single `sh`
//! invocation so that the shell performs `~` expansion and so the working
//! directory carries from the `clone` into the install step.

use cella_backend::{ContainerBackend, ExecOptions};

/// Boxed, thread-safe error returned by [`install_dotfiles`].
type DotfilesError = Box<dyn std::error::Error + Send + Sync>;

/// Candidate install scripts probed, in order, when no explicit install
/// command is given.
///
/// Order and contents are a verbatim port of `installCommands` in the official
/// `dotfiles.ts:11-20`. The task brief listed a shorter 6-entry set, but that
/// matches only the CLI help text; the actual source (and the spec's verifier
/// correction) uses these eight entries, so we follow the source for parity.
/// Flip this back to the 6-entry list if literal help-text parity is ever
/// preferred over runtime parity.
const INSTALL_COMMAND_CANDIDATES: [&str; 8] = [
    "install.sh",
    "install",
    "bootstrap.sh",
    "bootstrap",
    "script/bootstrap",
    "setup.sh",
    "setup",
    "script/setup",
];

/// Build the install branch for an explicit install command.
///
/// Tries `./<cmd>` first (relative to the freshly cloned repo root), then a
/// bare `<cmd>` on `PATH`, `chmod +x`-ing whichever exists before running it.
/// Mirrors `dotfiles.ts:50-71`.
fn explicit_install_branch(install_command: &str) -> String {
    let cmd = install_command;
    format!(
        r#"if [ -f "./{cmd}" ]; then
  [ -x "./{cmd}" ] || chmod +x "./{cmd}"
  "./{cmd}"
elif [ -f "{cmd}" ]; then
  [ -x "{cmd}" ] || chmod +x "{cmd}"
  "{cmd}"
else
  echo "Could not locate '{cmd}'"
  exit 126
fi"#
    )
}

/// Build the autodetect branch run when no explicit install command is given.
///
/// Probes [`INSTALL_COMMAND_CANDIDATES`] in order, `chmod +x`-ing and running
/// the first that exists. If none exist the script does nothing and succeeds.
/// Mirrors the loop in `dotfiles.ts:80-107` minus the symlink fallback, which
/// is intentionally out of scope for this module.
fn autodetect_install_branch() -> String {
    let candidates = INSTALL_COMMAND_CANDIDATES.join(" ");
    format!(
        r#"installCommand=""
for f in {candidates}; do
  if [ -e "$f" ]; then
    installCommand="$f"
    break
  fi
done
if [ -n "$installCommand" ]; then
  [ -x "$installCommand" ] || chmod +x "$installCommand"
  "./$installCommand"
fi"#
    )
}

/// Build the full POSIX shell script that clones `repository` into
/// `target_path` and runs the install command.
///
/// `target_path` is interpolated **literally and unquoted** at all three sites
/// (`[ -e ]`, `git clone`, `cd`), exactly as the official `dotfiles.ts:46-48,77`
/// does. This lets the shell expand a leading `~`/`~user` via the passwd
/// database even when `HOME` is unset (e.g. `userEnvProbe=none`), which a
/// quoted `"$HOME/..."` substitution could not. The clone is skipped when the
/// target already exists, matching `dotfiles.ts:46/77`.
fn build_script(repository: &str, install_command: Option<&str>, target_path: &str) -> String {
    let install_branch = install_command.map_or_else(autodetect_install_branch, |cmd| {
        explicit_install_branch(cmd)
    });
    format!(
        r#"set -e
command -v git >/dev/null 2>&1 || {{ echo "git not found"; exit 1; }}
[ -e {target_path} ] || git clone --depth 1 "{repository}" {target_path}
cd {target_path}
{install_branch}
"#
    )
}

/// Clone and install a dotfiles repository inside a running container.
///
/// Runs entirely as `remote_user` (never root) via a single `sh -c`
/// invocation, so the shell expands a leading `~` in `target_path` (via the
/// passwd database, even when `HOME` is unset) and the clone's working
/// directory carries into the install step. `env` (lifecycle `KEY=VALUE`
/// entries) is forwarded to the exec environment, matching the official tool.
///
/// `repository` is passed to `git clone` verbatim — owner/repo shorthand
/// normalization is the caller's responsibility. If `install_command` is
/// `None`, the first existing script in [`INSTALL_COMMAND_CANDIDATES`] is run;
/// if none exist this succeeds without doing anything.
///
/// # Errors
///
/// Returns an error if the exec transport fails or the install script exits
/// non-zero. The caller is expected to treat a dotfiles failure as a
/// non-fatal warning.
pub async fn install_dotfiles(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
    repository: &str,
    install_command: Option<&str>,
    target_path: &str,
    env: &[String],
) -> Result<(), DotfilesError> {
    let script = build_script(repository, install_command, target_path);
    let opts = ExecOptions {
        cmd: vec!["sh".to_string(), "-c".to_string(), script],
        user: Some(remote_user.to_string()),
        env: Some(env.to_vec()),
        working_dir: None,
    };
    let result = client.exec_command(container_id, &opts).await?;
    if result.exit_code != 0 {
        return Err(format!(
            "dotfiles install failed (exit {}): {}",
            result.exit_code,
            result.stderr.trim()
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        INSTALL_COMMAND_CANDIDATES, autodetect_install_branch, build_script,
        explicit_install_branch,
    };

    #[test]
    fn autodetect_candidate_order_matches_official_source() {
        // Verbatim order from dotfiles.ts:11-20 (eight entries).
        assert_eq!(
            INSTALL_COMMAND_CANDIDATES,
            [
                "install.sh",
                "install",
                "bootstrap.sh",
                "bootstrap",
                "script/bootstrap",
                "setup.sh",
                "setup",
                "script/setup",
            ]
        );
    }

    #[test]
    fn autodetect_branch_emits_candidates_in_order() {
        let branch = autodetect_install_branch();
        assert!(branch.contains(
            "for f in install.sh install bootstrap.sh bootstrap script/bootstrap setup.sh setup script/setup;"
        ));
        // First-match-wins loop plus chmod fallback.
        assert!(branch.contains("break"));
        assert!(branch.contains("chmod +x"));
    }

    #[test]
    fn explicit_branch_tries_relative_then_path_with_chmod() {
        let branch = explicit_install_branch("install.sh");
        assert!(branch.contains(r#"[ -f "./install.sh" ]"#));
        assert!(branch.contains(r#"elif [ -f "install.sh" ]"#));
        assert!(branch.contains(r#"chmod +x "./install.sh""#));
        assert!(branch.contains(r#"chmod +x "install.sh""#));
        assert!(branch.contains("exit 126"));
    }

    #[test]
    fn script_passes_tilde_target_literally_and_unquoted() {
        let script = build_script("octocat/dotfiles", None, "~/dotfiles");
        // Target is interpolated literally and UNQUOTED so the shell expands
        // the leading ~ itself (dotfiles.ts:46-48,77).
        assert!(script.contains(r#"git clone --depth 1 "octocat/dotfiles" ~/dotfiles"#));
        assert!(script.contains("cd ~/dotfiles"));
        // Idempotent clone guard from dotfiles.ts:77.
        assert!(script.contains("[ -e ~/dotfiles ] ||"));
        // Tilde must NOT be substituted to $HOME or quoted.
        assert!(!script.contains("$HOME"));
        assert!(!script.contains(r#""~/dotfiles""#));
        // git presence check is present (a missing git fails the exec).
        assert!(script.contains("command -v git"));
    }

    #[test]
    fn script_explicit_command_uses_explicit_branch() {
        let script = build_script("https://example.com/d.git", Some("setup"), "/home/me/d");
        assert!(script.contains(r#"git clone --depth 1 "https://example.com/d.git" /home/me/d"#));
        assert!(script.contains(r#"[ -f "./setup" ]"#));
        // Autodetect loop must NOT appear when an explicit command is given.
        assert!(!script.contains("for f in install.sh"));
    }
}
