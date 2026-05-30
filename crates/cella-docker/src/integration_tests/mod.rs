//! Integration tests using `#[runtime_test]` for runtime detection.
//!
//! Tests that need Docker use `#[runtime_test(docker)]` and skip
//! gracefully when Docker is unavailable.

mod claude_seed_tests;
mod network_tests;
