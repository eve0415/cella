# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test

```sh
cargo fmt --all -- --check                                            # format check
cargo clippy --workspace --all-targets -- -D warnings -D clippy::all  # lint (all warnings are errors)
cargo test --workspace                                                # unit tests
cargo insta test --workspace --check --unreferenced=reject            # snapshot tests
```

Integration tests require Docker and are feature-gated:

```sh
cargo test -p cella-features -p cella-daemon -p cella-compose --features integration-tests
```

Update snapshots with `cargo insta review`.

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
- Error types: thiserror for custom errors, miette for user-facing diagnostics
- Async: tokio runtime, bollard for Docker API, gix for git operations
- No backward compatibility constraints — refactor freely when improving code
- Create new crates when needed to maintain clean boundaries between concerns
- Research the codebase, crate READMEs, and existing patterns before proposing changes

## Architecture

Rust workspace (edition 2024, MSRV 1.94.0) with 19 crates in `crates/`. Three-tier structure:

- **Tier 1 (CLI):** cella-cli — binary entry point, delegates to library crates
- **Tier 2 (Domain):** cella-docker, cella-compose, cella-orchestrator, cella-config, cella-features, cella-git, cella-daemon, cella-agent, cella-env, cella-doctor, cella-container, cella-templates
- **Tier 3 (Foundation):** cella-backend, cella-port, cella-codegen, cella-network, cella-protocol, cella-jsonc

Backend-agnostic design: cella-backend defines traits, cella-docker and cella-container implement them. Hooks pattern (PruneHooks, ComposeUpHooks, UpHooks) bridges CLI-owned operations into the orchestrator without circular dependencies.

## Commits & PRs

- Conventional commits: `feat:`, `fix:`, `refactor:`, `chore:`, etc.
- Tiny atomic commits, one concern per commit
- Independent branches per feature, rebase before PR, no merge commits on main
- Breaking changes are acceptable

## CI Workflows

GitHub Actions in `.github/workflows/`. Every workflow step must have a `name` field with blank lines between steps.
