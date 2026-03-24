# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Compatibility

- **Drop-in replacement** for the devcontainer CLI and VS Code devcontainer extension — spec compatibility is non-negotiable, must behave identically
- No backward compatibility needed: cella has no prior releases or users; do not add compat shims, feature flags, or deprecation paths
- Check original devcontainer docs/specs before asking design questions

## Commands

```sh
cargo check --workspace                   # type-check
cargo build                               # build
cargo test --workspace                    # all tests
cargo test -p <crate>                     # single crate tests
cargo test -p <crate> -- <name>           # single test
cargo clippy --workspace --all-targets -- -D warnings -D clippy::all  # lint
cargo fmt --all                           # format
cargo insta review                        # accept/reject snapshot changes
```

Integration tests (requires Docker): `cargo test --workspace -- --ignored`

## Conventions

- Edition 2024, resolver v2
- Clippy pedantic + nursery (warn), `unsafe_code` denied
- Clippy allows: `must_use_candidate`, `similar_names`
- Dependencies pinned to exact versions (x.y.z) in `[workspace.dependencies]`
- Conventional commits (`feat:`, `fix:`, `refactor:`, `docs:`, `test:`, `chore:`), signed commits required
- Error types: `thiserror`; user-facing diagnostics: `miette`

## Testing

- Unit tests inline with `#[cfg(test)]` modules
- `#[tokio::test]` for async tests
- `#[ignore]` for Docker-dependent integration tests
- Snapshot tests use `insta` — run `cargo insta review` after changes

## Build-time codegen

`cella-config/build.rs` generates typed Rust structs from the devcontainer JSON Schema (`cella-config/schemas/devContainer.base.schema.json`) via `cella-codegen`. After any schema or codegen changes, run `cargo insta review` to update snapshots.

## Cross-compilation

cella-agent is cross-compiled to static musl binaries (x86_64 and aarch64) for container portability. Requires musl-tools.

## Per-crate instructions

`crates/cella-config/CLAUDE.md` and `crates/cella-codegen/CLAUDE.md` contain crate-specific guidance.
