pub mod devcontainer;
pub mod settings;

// Re-export all devcontainer modules at crate root for backward compatibility.
pub use devcontainer::*;
pub use settings::{CellaSettings, ClaudeCodeSettings};

/// Types and validators generated from the devcontainer JSON Schema.
#[allow(
    unused_variables,
    deprecated,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
pub mod schema {
    include!(concat!(env!("OUT_DIR"), "/generated.rs"));
}
