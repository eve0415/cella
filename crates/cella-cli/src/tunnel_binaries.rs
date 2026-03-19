//! Embedded tunnel-server binaries for container deployment.
//!
//! At release time, cross-compiled static musl binaries are placed at known paths
//! and embedded via `include_bytes!`. For development, empty stubs are used and
//! tunnel-based forwarding logs a warning.

#[cfg(feature = "embed-tunnel-server")]
pub const TUNNEL_SERVER_X86_64: &[u8] =
    include_bytes!("../../../target/tunnel-server/x86_64/cella-tunnel-server");
#[cfg(feature = "embed-tunnel-server")]
pub const TUNNEL_SERVER_AARCH64: &[u8] =
    include_bytes!("../../../target/tunnel-server/aarch64/cella-tunnel-server");

#[cfg(not(feature = "embed-tunnel-server"))]
pub const TUNNEL_SERVER_X86_64: &[u8] = &[];
#[cfg(not(feature = "embed-tunnel-server"))]
pub const TUNNEL_SERVER_AARCH64: &[u8] = &[];
