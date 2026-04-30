//! Docker credential resolution for OCI registry authentication.
//!
//! Delegates to `cella_oci::auth` which contains the canonical implementation.

pub use cella_oci::auth::*;
