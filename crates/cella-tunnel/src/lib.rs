// Async mutex guards in tokio code have inherently wider scopes.
#![allow(clippy::significant_drop_tightening)]

pub mod client;
pub mod daemon;
mod error;
pub mod git_credential;
pub mod mux;
pub mod server;
pub mod ssh_agent;
pub mod tunnel;

pub use error::CellaTunnelError;
