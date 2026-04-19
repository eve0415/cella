# Testing Strategy

## Overview

cella uses a layered testing approach:

- **Unit tests** — fast, inline with the code, no external dependencies
- **Snapshot tests** — schema codegen and feature resolution, via [insta](https://insta.rs/)
- **Integration tests** — feature-gated, require Docker and network access

## Unit Tests

Unit tests live inline in each module using `#[cfg(test)]` — not in a separate `tests/` directory:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        // ...
    }
}
```

Run all unit tests:

```sh
cargo test --workspace
```

## Snapshot Tests

`cella-codegen` and `cella-features` produce deterministic output (generated Rust structs, Dockerfile emission, feature resolution) that is validated against stored snapshots using insta.

Run snapshot checks:

```sh
cargo insta test --workspace --check --unreferenced=reject
```

Review and accept intentional changes:

```sh
cargo insta review
```

If `cargo-insta` is not installed in a dev container, run `cargo test` to verify snapshot correctness and then review/accept from the host.

## Integration Tests

Tests that require Docker or network access (OCI registry fetches, container lifecycle) are gated behind a Cargo feature flag:

```rust
#[cfg(feature = "integration-tests")]
#[tokio::test]
async fn fetch_from_ghcr() {
    // ...
}
```

Run integration tests locally (requires Docker):

```sh
cargo test -p cella-features -p cella-daemon -p cella-compose --features integration-tests
```

These tests run automatically in CI via the **Integration Test** job, which authenticates to `ghcr.io` and produces a combined coverage report via `cargo llvm-cov`.

### Architecture-aware tests

OCI tests use `test_platform()` instead of hardcoding `"amd64"`. This maps `std::env::consts::ARCH` to OCI platform names (`x86_64` → `amd64`, `aarch64` → `arm64`), so tests work on both Intel and ARM runners.

### Locally-only tests

One test (`credential_helper_invocation` in `cella-features`) requires `docker-credential-desktop` on `PATH` and uses `#[ignore]`. It does not run in CI. Run it manually:

```sh
cargo test -p cella-features -- --ignored credential_helper
```

## Mocking Patterns

External dependencies (Docker, git CLI) are abstracted behind traits so unit tests can supply lightweight fakes instead of touching real infrastructure:

```rust
// In the library crate:
pub trait ContainerRuntime {
    async fn create(&self, config: &Config) -> Result<ContainerId>;
    async fn start(&self, id: &ContainerId) -> Result<()>;
    // ...
}

// In tests:
struct MockRuntime { /* ... */ }
impl ContainerRuntime for MockRuntime {
    // Return predefined values
}
```

`cella-orchestrator` ships a `NoOpHooks` implementation of its `UpHooks` / `ComposeUpHooks` / `PruneHooks` traits for tests that exercise the orchestrator without a real CLI or daemon attached.

## Adding New Tests

1. **Unit test**: add a `#[cfg(test)]` module in the same file as the code under test.
2. **Integration test**: wrap the test in `#[cfg(feature = "integration-tests")]`. Add the feature flag to the crate's `Cargo.toml` if not already present.
3. **Async tests**: use `#[tokio::test]` for async test functions.
4. **Snapshots**: use `insta::assert_snapshot!` / `insta::assert_json_snapshot!`; run `cargo insta review` to accept.
5. **Architecture**: use `test_platform()` instead of hardcoding `"amd64"` in OCI or image-architecture tests.
6. **Regression tests**: every bugfix should land with a failing test from the bug and the fix in the same commit (or split as the fix + the test, but the test should precede or accompany the code).

## CI Coverage

The Integration Test job aggregates coverage across unit and integration runs with `cargo llvm-cov`. A per-PR sticky comment posts workspace total and per-crate coverage so reviewers can see where a change regresses test coverage.
