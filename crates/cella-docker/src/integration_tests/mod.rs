//! Integration tests gated on `--features integration-tests`.
//!
//! These tests require a reachable Docker daemon and are marked
//! `#[ignore = "requires Docker daemon"]` so they don't run by default.
//! Execute with:
//! ```sh
//! cargo test -p cella-docker --features integration-tests -- --ignored
//! ```

mod network_tests;
