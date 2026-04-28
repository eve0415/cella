# Contributing

## Prerequisites

- **Rust toolchain** — install via [rustup](https://rustup.rs/). Cella's MSRV is **1.95.0** (edition 2024), pinned in the workspace `Cargo.toml`.
- **Docker** — needed for integration tests and for running cella itself. Unit tests do not require it.

## Building

```sh
cargo build --workspace
```

## Running Tests

Unit tests (no external dependencies):

```sh
cargo test --workspace
```

Integration tests (requires Docker, feature-gated):

```sh
cargo test -p cella-features -p cella-daemon -p cella-compose --features integration-tests
```

Snapshot tests (insta):

```sh
cargo insta test --workspace --check --unreferenced=reject
```

Update snapshots with `cargo insta review`. If `cargo-insta` is not installed in a dev container, run `cargo test` to verify snapshot correctness, then review/accept from the host.

## Code Style

- **Clippy**: We use `clippy::pedantic` and `clippy::nursery` workspace-wide, plus `unsafe_code = deny` and `unused_qualifications = deny`. Run `cargo clippy --workspace --all-features --all-targets -- -D warnings -D clippy::all` and fix every warning before submitting — CI treats warnings as errors.
- **Formatting**: Run `cargo fmt --all` before committing. CI will reject unformatted code.
- **Unsafe code**: Denied at the workspace level. Do not use `unsafe`.
- **Don't suppress**: No `#[allow(clippy::...)]`, no `_`-prefixed unused variables. Fix the warning or delete the dead code.
- **`too_many_lines`**: Clippy's limit is 100 LOC per function. Extract helpers before hitting the limit — don't reach for `#[allow]`.

## Testing Conventions

- Unit tests are colocated with the code they test using `#[cfg(test)] mod tests` — not separate `tests/` directories.
- Add unit tests when implementing new code.
- Add regression tests when fixing bugs.
- Use `test_platform()` (not hardcoded arch strings) in OCI or architecture-aware tests.

## Commit Conventions

We follow [Conventional Commits](https://www.conventionalcommits.org/):

- `feat:` — new feature or capability
- `fix:` — bug fix
- `chore:` — maintenance, dependencies, tooling
- `docs:` — documentation changes
- `refactor:` — code restructuring without behavior change
- `test:` — adding or fixing tests
- `ci:` / `cd:` — CI/CD pipeline changes

Keep commits tiny and atomic — one concern per commit. Rebase (don't merge) onto `main` before opening a PR; no merge commits land on `main`.

## Pull Requests

1. Create a feature branch from `main`.
2. Make your changes with clear, focused commits.
3. Ensure the following all pass locally (they match CI exactly):
   - `cargo fmt --all -- --check`
   - `cargo clippy --workspace --all-features --all-targets -- -D warnings -D clippy::all`
   - `cargo test --workspace`
   - `cargo insta test --workspace --check --unreferenced=reject`
4. Open a PR against `main` with a clear description of the change.
