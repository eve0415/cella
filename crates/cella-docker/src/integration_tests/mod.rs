//! Integration tests using `#[runtime_test]` for runtime detection.
//!
//! Tests that need Docker use `#[runtime_test(docker)]` and skip
//! gracefully when Docker is unavailable.

mod network_tests;
