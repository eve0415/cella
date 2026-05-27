# cella-testing

> Runtime detection and test harness for integration tests that require container runtimes.

Part of the [cella](../../README.md) workspace.

## Overview

cella-testing provides the `#[runtime_test]` attribute macro and runtime detection functions that let integration tests skip gracefully when required runtimes (Docker, Podman, Compose, etc.) are unavailable. Tests always compile and run everywhere — they print a skip message and return early when detection fails.

Detection results are cached in static `OnceLock`/`OnceCell` cells so repeated checks within a test run hit no external processes. All probes time out after 5 seconds.

## Architecture

### Key Types

- `#[runtime_test]` — attribute macro (re-exported from `cella-test-macros`) that wraps a test function with runtime availability guards
- `detect::*_available()` / `detect::*_available_sync()` — async and sync detection functions for each supported runtime
- `detect::container_runtime_available()` — returns `true` if any container runtime is reachable (default when `#[runtime_test]` has no arguments)
- `detect::container_runtime_available_except()` — same, but excludes specific runtimes (used by negated requirements like `#[runtime_test(!podman)]`)

### Modules

| Module | Purpose |
|--------|---------|
| `lib` | Re-exports `#[runtime_test]` from `cella-test-macros`, declares `detect` module |
| `detect` | Runtime probing via Docker API ping, command execution, and TCP connect; `detect_fn!` macro generates cached async+sync pairs for each runtime |

### Supported Runtimes

`docker`, `compose`, `buildx`, `podman`, `apple_container`, `orbstack`, `colima`, `lima`, `network`

### Macro Usage

```rust
#[runtime_test]                        // any container runtime
#[runtime_test(docker)]                // Docker specifically
#[runtime_test(docker, compose)]       // Docker AND Compose
#[runtime_test(!podman)]               // any runtime except Podman
#[runtime_test(docker, flavor = "multi_thread")]  // with tokio flavor
```

## Crate Dependencies

**Depends on:** [cella-test-macros](../cella-test-macros) (proc macro), bollard, tokio

**Depended on by (dev):** [cella-daemon](../cella-daemon), [cella-docker](../cella-docker), [cella-features](../cella-features), [cella-templates](../cella-templates)

## Testing

```sh
cargo test -p cella-testing
```

## Development

When adding a new runtime detector:

1. Add an `async fn check_*()` probe in `detect.rs`
2. Invoke `detect_fn!` to generate the cached async+sync pair
3. Add the runtime name to `KNOWN_RUNTIMES` in `cella-test-macros/src/lib.rs`
4. Include it in `container_runtime_available()` and `container_runtime_available_except()` if it qualifies as a container runtime
