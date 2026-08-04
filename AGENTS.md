# AGENTS.md

Rust workspace (edition 2024, MSRV 1.97.1). Crates live in `crates/`.

## Gates — run before declaring any change complete

```sh
cargo fmt --all
cargo clippy -p <crate> --all-targets -- -D warnings -D clippy::all
cargo test -p <crate>
```

Full-workspace equivalents (`--workspace`) take minutes; run per-crate while iterating and the full sweep once at the end.

## Hard rules

- **No `#[allow(clippy::...)]`.** Fix the warning or restructure the code. Workspace lints are clippy::pedantic + clippy::nursery, `unsafe_code` denied, `unused_qualifications` denied.
- **No `_`-prefixed unused variables.** Delete dead code entirely.
- Clippy `too_many_lines` caps functions at 100 LOC — extract helpers rather than suppressing.
- `clippy::significant_drop_tightening` is on: inline `.lock().await.method()` instead of binding the guard when only one call is needed.
- Dependencies are pinned exactly (`tokio = "=1.53.1"`). Match that format, and commit `Cargo.lock` in the same commit as any dependency change.
- SHA-256 hex output: `hex::encode(hasher.finalize())`. `format!("{:x}", ...)` does not compile against `GenericArray`.
- Errors: `thiserror` for library errors, `miette` for user-facing diagnostics.
- Async is tokio; Docker via bollard; git via gix.

## Testing

- Unit tests colocate in source (`#[cfg(test)] mod tests`), not separate directories.
- Integration tests use `#[runtime_test]` from `cella-testing`; they compile always and skip when the runtime is unavailable.
- Add a regression test with every bug fix, and unit tests with new code.
- `cargo-insta` is often absent in dev containers. `cargo test` still checks snapshot correctness; do not claim `cargo insta` ran if it is not installed.

## Commits

Conventional commits (`feat:`, `fix:`, `perf:`, `refactor:`, `chore:`). Tiny atomic commits, one concern each. Subject plus at most ~3 short lines of body — no measurements or narrative in the message.

## Performance work

Hot paths are per-item/per-file/per-node work: loops over large inputs, recursive traversals, per-request and per-entry paths. Startup, config/CLI parsing, tests, and error paths are cold — clarity wins there.

Claimed wins need evidence: a benchmark or an allocation count before and after. Benches use divan (`divan.workspace = true`, `[[bench]] harness = false`), and `divan::AllocProfiler` as `#[global_allocator]` in the bench file gives per-bench allocation counts. `unsafe` is a last resort and requires profiling proof.

Correctness always outranks performance. Never change observable behavior to win a benchmark.
