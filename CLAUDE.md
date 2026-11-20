# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test

```sh
cargo fmt --all -- --check                                            # format check
cargo clippy --workspace --all-features --all-targets -- -D warnings -D clippy::all  # lint (all warnings are errors)
cargo test --workspace                                                # all tests (integration tests skip if runtime unavailable)
cargo insta test --workspace --check --unreferenced=reject            # snapshot tests
cargo gevatter check -- --workspace --all-targets                     # lifetime annotations (local only, not run in CI)
```

Integration tests use `#[runtime_test]` from `cella-testing` for runtime detection. They always compile and skip gracefully when the required runtime (Docker, compose, network, etc.) is unavailable. No special flags needed — `cargo test --workspace` runs everything.

Update snapshots with `cargo insta review`.

**Note:** the dev container installs `cargo-insta` via `postCreateCommand`, so the snapshot gate runs here. Update snapshots with `cargo insta review`.

The dev container also installs `cargo-gevatter` (outside it, install the version pinned in `.devcontainer/devcontainer.json` with `cargo install cargo-gevatter@<version>`, then run `cargo gevatter toolchain install`). `cargo gevatter fix -- --workspace --all-targets` applies its suggestions; run `cargo fmt --all` afterwards, because it writes inline `where` clauses.

## Testing

- Unit tests colocated in source files (`#[cfg(test)] mod tests`) — not separate test directories
- Add unit tests when implementing new code
- Add regression tests when fixing bugs
- Use `test_platform()` (not hardcoded arch strings) in OCI/architecture-aware tests

## Spec Compliance

cella is a drop-in replacement for the devcontainer CLI and VS Code devcontainer extension. All commands, options, and behaviors must match the original tools. Before making design decisions or asking questions:

1. Research the devcontainer spec at containers.dev
2. Check the official CLI source and its GitHub issues
3. Check VS Code devcontainer extension behavior and its issues
4. Look for known bugs to fix proactively rather than replicate

Always research before suggesting changes or asking the user questions. Use `/spec-research` for systematic investigation.

## Code Conventions

- No `#[allow(clippy::...)]` annotations — fix the warning or restructure the code
- No `_`-prefixed unused variables — delete dead code entirely
- Workspace lints: clippy::pedantic + clippy::nursery (warn), unsafe_code (deny), unused_qualifications (deny)
- `clippy::significant_drop_tightening` is active — inline mutex `.lock().await.method()` calls instead of binding the guard to a variable when only one method call is needed
- Clippy `too_many_lines` limit is 100 LOC per fn — extract helpers before hitting it, don't reach for `#[allow]`
- SHA-256 hex output: `hex::encode(hasher.finalize())` — `format!("{:x}", ...)` does not compile against `GenericArray`
- Error types: thiserror for custom errors, miette for user-facing diagnostics
- Async: tokio runtime, bollard for Docker API, gix for git operations
- No backward compatibility constraints — refactor freely when improving code
- Create new crates when needed to maintain clean boundaries between concerns
- Research the codebase, crate READMEs, and existing patterns before proposing changes

## Architecture

Rust workspace (edition 2024, MSRV 1.100.0) with crates in `crates/`. Three-tier structure:

- **Tier 1 (CLI):** cella-cli — binary entry point, delegates to library crates
- **Tier 2 (Domain):** cella-docker, cella-compose, cella-orchestrator, cella-config, cella-features, cella-git, cella-daemon, cella-agent, cella-env, cella-doctor, cella-container, cella-templates
- **Tier 3 (Foundation):** cella-backend, cella-port, cella-codegen, cella-network, cella-protocol, cella-jsonc
- **Testing:** cella-testing (runtime detection + `#[runtime_test]` macro), cella-test-macros (proc macro)

Backend-agnostic design: cella-backend defines traits, cella-docker and cella-container implement them. Hooks pattern (PruneHooks, ComposeUpHooks, UpHooks) bridges CLI-owned operations into the orchestrator without circular dependencies.

Key runtime constraints:
- The `cella-agent` Docker volume is shared across ALL containers — changes affect every running container
- Daemon state (port allocations, registrations) is in-memory only, lost on daemon restart
- Docker container labels and env vars are immutable after creation — don't design around updating them
- The agent daemon runs as root under the supervisor loop that `spawn_agent_supervisor()` (`cella-backend::agent`) launches, which logs to `/run/cella-agent.log` (falling back to `/dev/null`, emptied past 1 MiB); a root entrypoint starts it, a non-root entrypoint starts nothing, `restart_agent_in_container()` (exec as root) kills the daemon, removes `/tmp/.cella/proxy-ready` and starts a supervisor if none runs, and `ensure_agent_running()` only starts a missing supervisor — both are blocking execs that return when the script exits (the supervisor is launched with its streams redirected), scan `/proc` with `tr`, `read` and `kill` (no procps needed), match cmdlines by the prefixes `/cella/bin/cella-agent daemon` and `sh -c : cella-agent-supervisor`, which the script's own `sh -c [ -x …` argv never matches, and act only on processes whose `/proc/<pid>/mountinfo` root mount ID equals their own (`--pid=host` shows other containers' processes; `ns/mnt` is unreadable across users without `CAP_SYS_PTRACE`)
- Each daemon holds an exclusive lock on `/run/cella-agent.lock`, so a duplicate daemon waits idle and a non-root daemon cannot open the root-owned lock and exits; a root daemon that cannot use the lock warns and runs unlocked
- The agent reconnects indefinitely on initial daemon miss and after daemon restart/binary upgrade — don't assume a fresh agent means a fresh daemon

## Commits & PRs

- Conventional commits: `feat:`, `fix:`, `refactor:`, `chore:`, etc.
- Tiny atomic commits, one concern per commit
- Independent branches per feature, rebase before PR, no merge commits on main
- Breaking changes are acceptable

## CI Workflows

GitHub Actions in `.github/workflows/`. Every workflow step must have a `name` field with blank lines between steps.
