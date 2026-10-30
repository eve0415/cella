//! Network configuration, proxy environment detection, and rule matching for cella.
//!
//! This crate provides:
//! - Configuration types for proxy settings and network blocking rules
//! - A glob-based rule matching engine for domain and path filtering
//! - Host proxy environment variable auto-detection

pub mod authority;
pub mod ca;
pub mod config;
pub mod proxy_env;
pub mod rules;
pub mod validate;

pub use authority::{AuthorityError, HostPort};
pub use config::{
    NetworkConfig, NetworkMode, NetworkRule, NetworkRulePolicy, ProxyConfig, ResponseSpec,
    RuleAction, status_forbids_body,
};
pub use proxy_env::ProxyEnvVars;
pub use rules::RuleMatcher;
pub use validate::NetworkConfigError;
