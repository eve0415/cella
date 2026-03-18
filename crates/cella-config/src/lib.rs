mod error;

pub use error::CellaConfigError;

/// Types generated from the devcontainer JSON Schema.
pub mod schema {
    include!(concat!(env!("OUT_DIR"), "/generated.rs"));
}
