# Testing Strategy

## Overview

cella uses a layered testing approach:

- **Unit tests** — fast, inline, no external dependencies
- **Integration tests** — per-crate `tests/` directories, may require Docker

## Unit Tests

Unit tests live inline in each module using `#[cfg(test)]`:

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

## Integration Tests

Tests that require Docker or network access (e.g. OCI registry fetches) are
gated behind a Cargo feature flag:

```rust
#[cfg(feature = "integration-tests")]
#[tokio::test]
async fn fetch_from_ghcr() {
    // ...
}
```

Run integration tests locally (requires Docker):

```sh
cargo test -p cella-features -p cella-daemon -p cella-credential-proxy --features integration-tests
```

These tests run automatically in CI via the **Integration Test** job, which
authenticates to `ghcr.io` and collects coverage.

### Architecture-aware tests

OCI tests use `test_platform()` instead of hardcoding `"amd64"`. This maps
`std::env::consts::ARCH` to OCI platform names (`x86_64` → `amd64`,
`aarch64` → `arm64`), so tests work on both Intel and ARM runners.

### Locally-only tests

One test (`credential_helper_invocation` in `cella-features`) requires
`docker-credential-desktop` on `PATH` and uses `#[ignore]`. It does not run
in CI. Run it manually:

```sh
cargo test -p cella-features -- --ignored credential_helper
```

## Mocking Patterns

External dependencies (Docker, git CLI) are abstracted behind traits. This allows unit tests to use mock implementations without touching real infrastructure:

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

## Adding New Tests

1. **Unit test**: Add a `#[cfg(test)]` module in the same file as the code being tested
2. **Integration test**: Use `#[cfg(feature = "integration-tests")]` for tests needing Docker or network. Add the feature to the crate's `Cargo.toml` if not already present
3. **Async tests**: Use `#[tokio::test]` for async test functions
4. **Architecture**: Use `test_platform()` instead of hardcoding `"amd64"` in OCI tests

## Test Organization by Crate

| Crate | Unit Tests | Integration Tests |
|-------|-----------|-------------------|
| cella-config | Config parsing, layer merging | — |
| cella-docker | — | Container lifecycle, image builds |
| cella-git | Path manipulation, config parsing | Worktree operations |
| cella-port | Port allocation logic | Port binding |
| cella-agent | Preset resolution | Full sandbox lifecycle |
| cella-cli | Argument parsing | End-to-end command tests |
