//! Unified cella daemon: credential forwarding, port management, and browser handling.
//!
//! Single daemon that manages credential forwarding, port forwarding, and
//! browser-open requests from in-container agents.

pub mod browser;
pub mod clipboard;
pub mod control_server;
pub mod credential;
pub mod credential_proxy;
pub mod credential_resolver;
pub mod daemon;
mod error;
pub mod health;
pub mod logging;
pub mod management;
pub mod orbstack;
pub mod phantom_registry;
pub mod port_manager;
pub mod proxy;
pub mod shared;
pub mod ssh_proxy;
pub mod stream_bridge;
pub mod task_manager;
pub mod tunnel;

pub use error::CellaDaemonError;
