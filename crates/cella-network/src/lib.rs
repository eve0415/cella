//! Network configuration, proxy environment detection, and rule matching for cella.
//!
//! This crate provides:
//! - Configuration types for proxy settings and network blocking rules
//! - A glob-based rule matching engine for domain and path filtering
//! - Host proxy environment variable auto-detection
//! - Rule merging from multiple configuration sources

pub mod ca;
pub mod config;
pub mod merge;
pub mod proxy_env;
pub mod rules;

pub use config::{NetworkConfig, NetworkMode, NetworkRule, ProxyConfig, RuleAction};
pub use merge::merge_network_configs;
pub use proxy_env::ProxyEnvVars;
pub use rules::RuleMatcher;
