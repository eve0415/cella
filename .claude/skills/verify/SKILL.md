---
name: verify
description: Run the full CI verification suite locally (format, lint, test, snapshots). Use before committing or to check if changes pass CI.
---

Run all CI checks in sequence, stopping on first failure. Report each step's result clearly.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings -D clippy::all
cargo test --workspace
cargo insta test --workspace --check --unreferenced=reject
```

If any step fails:
- For format failures: run `cargo fmt --all` to fix, then report what changed
- For clippy failures: fix the warnings (do not use `#[allow(clippy::...)]`)
- For test failures: investigate and fix the failing tests
- For snapshot failures: run `cargo insta review` and assess whether the new snapshots are correct
