//! Unified cella daemon: credential proxy + port manager + browser handler.
//!
//! Replaces the standalone `cella-credential-proxy` with a single daemon
//! that also manages port forwarding and browser-open requests from
//! in-container agents.

pub mod browser;
pub mod client;
pub mod control_server;
pub mod credential;
pub mod daemon;
mod error;
pub mod health;
pub mod logging;
pub mod management;
pub mod orbstack;
pub mod port_manager;
pub mod proxy;
pub mod shared;
pub mod task_manager;

pub use error::CellaDaemonError;
