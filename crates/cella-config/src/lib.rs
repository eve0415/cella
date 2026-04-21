pub mod cella_config;
pub mod devcontainer;
pub mod settings;

pub use cella_config::CellaConfig;

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
