//! A Wayland data-control clipboard server for cella containers.
//!
//! Clients that speak the Wayland wire protocol directly — `arboard`,
//! `wl-clipboard-rs`, and anything built on them — cannot see cella's
//! `/cella/bin` shell shims. This crate serves them a minimal compositor
//! surface: a seat plus the ext and wlr data-control managers, and nothing
//! graphical. Clipboard bytes come from a [`ClipboardSource`], which
//! `cella-agent` implements against the daemon control channel.

mod source;

pub use source::{ClipboardSource, SourceError};
