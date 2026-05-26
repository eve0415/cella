# cella-test-macros

> Proc macro crate providing `#[runtime_test]` for runtime-gated integration tests.

Part of the [cella](../../README.md) workspace.

## Overview

cella-test-macros defines the `#[runtime_test]` attribute macro. It wraps test functions with runtime detection checks so integration tests compile unconditionally but skip at runtime when the required container runtime (Docker, Podman, etc.) is unavailable.

The macro is not used directly — cella-testing re-exports it as `cella_testing::runtime_test`, which is the public API tests import.

## Usage

```rust
use cella_testing::runtime_test;

// Skip if no container runtime is available
#[runtime_test]
async fn test_container_lifecycle() {
    // ...
}

// Require specific runtimes
#[runtime_test(docker, compose)]
async fn test_compose_up() {
    // ...
}

// Exclude specific runtimes
#[runtime_test(!podman)]
async fn test_docker_only_feature() {
    // ...
}

// Set tokio test flavor
#[runtime_test(docker, flavor = "multi_thread")]
async fn test_parallel_containers() {
    // ...
}

// Sync tests work too
#[runtime_test(docker)]
fn test_sync_check() {
    // ...
}
```

### Known Runtimes

`docker`, `compose`, `buildx`, `podman`, `apple_container`, `orbstack`, `colima`, `lima`, `network`, `container_runtime`

### Behavior

| Invocation | Effect |
|------------|--------|
| `#[runtime_test]` | Checks `container_runtime_available()`, skips if false |
| `#[runtime_test(docker, compose)]` | Checks each named runtime, skips on first unavailable |
| `#[runtime_test(!podman)]` | Checks any runtime *except* podman is available |
| `#[runtime_test(docker, flavor = "multi_thread")]` | Runtime gate + custom tokio test flavor |

Mixing positive and negated requirements (e.g., `docker, !podman`) is a compile error.

## Architecture

### Key Types

- `RuntimeTestArgs` — parsed macro arguments: a list of `RuntimeRequirement`s and an optional tokio flavor
- `RuntimeRequirement` — a runtime name with a negation flag
- `RuntimeArg` — parse-level enum distinguishing runtime names from `flavor = "..."` key-value pairs
- `KNOWN_RUNTIMES` — compile-time allowlist of valid runtime names (unknown names are compile errors)

### Expansion

The macro expands into the original test function with a preamble injected at the top of the body:

- **Async functions** get `#[tokio::test]` (with optional flavor) and call `cella_testing::detect::*_available().await`
- **Sync functions** get `#[test]` and call `cella_testing::detect::*_available_sync()`

When a runtime is unavailable, the preamble prints a skip message and returns early.

## Crate Dependencies

**Depends on:** none (proc macro crate; uses syn, quote, proc-macro2)

**Depended on by:** [cella-testing](../cella-testing) (re-exports as `cella_testing::runtime_test`)

## Testing

```sh
cargo test -p cella-test-macros
```

The macro is primarily tested indirectly through integration tests across the workspace that use `#[runtime_test]`.

## Development

To add a new runtime:
1. Add the name to `KNOWN_RUNTIMES` in `src/lib.rs`
2. Add the corresponding `<name>_available()` and `<name>_available_sync()` detection functions in `cella-testing::detect`

The macro generates calls to `cella_testing::detect::<name>_available()` by convention, so the function name must match the runtime name exactly.
