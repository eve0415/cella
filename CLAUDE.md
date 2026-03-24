# cella

Early experimental drop-in replacement for the devcontainer CLI and VS Code devcontainer support.

- **Compatibility target**: must behave identically to the original devcontainer CLI and VS Code devcontainer extension — spec compatibility is non-negotiable
- **No backward compatibility**: cella has no prior releases or users; do not add compat shims, feature flags, or deprecation paths

## Commands

```sh
cargo check --workspace          # type-check
cargo build                      # build
cargo test --workspace           # all tests
cargo test -p <crate>            # single crate
cargo test -p <crate> -- <name>  # single test
cargo clippy --workspace --all-targets -- -D warnings -D clippy::all  # lint (CI denies warnings)
cargo fmt --all                  # format
cargo insta review               # accept/reject snapshot changes
```

## Conventions

- Edition 2024, resolver v2
- Clippy pedantic + nursery (warn), `unsafe_code` denied
- Clippy allows: `must_use_candidate`, `similar_names`
- Pinned dependency versions (exact x.y.z)
- Conventional commits, signed commits required
- Error handling: `thiserror` for error types, `miette` for user-facing diagnostics

## Crates

cella-agent, cella-cli, cella-codegen, cella-config, cella-credential-proxy, cella-daemon, cella-doctor, cella-docker, cella-env, cella-features, cella-git, cella-port

## Testing

- Unit tests inline with `#[cfg(test)]` modules
- `#[tokio::test]` for async tests
- `#[ignore]` for Docker-dependent tests
- Snapshot tests use `insta` — run `cargo insta review` after changes
