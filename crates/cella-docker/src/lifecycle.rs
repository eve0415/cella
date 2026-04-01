//! Lifecycle command parsing and execution.
//!
//! This module re-exports from `cella_backend::lifecycle` for backward
//! compatibility. New code should import from `cella_backend` directly.

pub use cella_backend::lifecycle::{
    LifecycleContext, OutputCallback, ParsedLifecycle, parse_lifecycle_command, run_lifecycle_phase,
};
