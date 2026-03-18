pub mod diagnostic;
pub mod discover;
mod error;
pub mod jsonc;
pub mod merge;
pub mod parse;
pub mod span;

pub use error::CellaConfigError;

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
