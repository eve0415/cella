//! Merging feature metadata into a unified container configuration.
//!
//! Implements two merge phases from the devcontainer spec:
//!
//! 1. **Feature-to-feature**: accumulates metadata from all resolved features
//!    in install order into a single [`FeatureContainerConfig`].
//! 2. **Feature-to-devcontainer**: merges the accumulated feature config with
//!    the user's `devcontainer.json` settings, respecting user-overrides.

mod devcontainer;
mod feature;
mod helpers;
mod validation;

pub use devcontainer::merge_with_devcontainer;
pub use feature::merge_features;
pub use validation::validate_options;
