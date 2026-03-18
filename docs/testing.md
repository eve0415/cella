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

Integration tests that require Docker or other external dependencies use the `#[ignore]` attribute:

```rust
#[test]
#[ignore] // requires Docker
fn container_lifecycle() {
    // ...
}
```

Run integration tests:

```sh
cargo test --workspace -- --ignored
```

Run all tests (unit + integration):

```sh
cargo test --workspace -- --include-ignored
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
2. **Integration test**: Create a file in the crate's `tests/` directory. Use `#[ignore]` if it requires Docker
3. **Async tests**: Use `#[tokio::test]` for async test functions

## Test Organization by Crate

| Crate | Unit Tests | Integration Tests |
|-------|-----------|-------------------|
| cella-config | Config parsing, layer merging | — |
| cella-docker | — | Container lifecycle, image builds |
| cella-git | Path manipulation, config parsing | Worktree operations |
| cella-port | Port allocation logic | Port binding |
| cella-agent | Preset resolution | Full sandbox lifecycle |
| cella-cli | Argument parsing | End-to-end command tests |
