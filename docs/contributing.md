# Contributing

## Prerequisites

- **Rust toolchain** — install via [rustup](https://rustup.rs/)
- **Docker** — optional, only needed for integration tests

## Building

```sh
cargo build --workspace
```

## Running Tests

Unit tests (no external dependencies):

```sh
cargo test --workspace
```

Integration tests (requires Docker):

```sh
cargo test --workspace -- --ignored
```

## Code Style

- **Clippy**: We use `clippy::pedantic` and `clippy::nursery` workspace-wide. Run `cargo clippy --workspace --all-targets` and fix all warnings before submitting.
- **Formatting**: Run `cargo fmt --all` before committing. CI will reject unformatted code.
- **Unsafe code**: Denied at the workspace level. Do not use `unsafe`.

## Commit Conventions

We follow [Conventional Commits](https://www.conventionalcommits.org/):

- `feat:` — new feature or capability
- `fix:` — bug fix
- `chore:` — maintenance, dependencies, tooling
- `docs:` — documentation changes
- `refactor:` — code restructuring without behavior change
- `test:` — adding or fixing tests

## Pull Requests

1. Create a feature branch from `main`
2. Make your changes with clear, focused commits
3. Ensure `cargo build --workspace`, `cargo test --workspace`, `cargo clippy --workspace --all-targets`, and `cargo fmt --all -- --check` all pass
4. Open a PR against `main` with a clear description of the change
