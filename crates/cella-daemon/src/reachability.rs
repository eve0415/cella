//! Host to container reachability probing.
//!
//! Port forwarding fails invisibly when the daemon cannot open a connection to
//! a container: the host-side listener still binds and still accepts, so every
//! forward looks healthy while each connection is dropped the moment the
//! upstream dial fails. This probe answers the one question that binding does
//! not: can this process still reach that container at all?

use std::io;
use std::time::Duration;

use cella_protocol::ContainerProbeResult;
use tokio::net::TcpStream;
use tracing::debug;

/// Port the probe dials.
///
/// Nothing is expected to listen here, and that is the point: a refusal proves
/// the container's network stack answered. Dialing a port the workspace owns
/// would open and immediately drop a real connection on it, which ends the
/// session of a single-client debugger and is logged as a broken client by
/// servers that record connections.
const PROBE_PORT: u16 = 1;

/// How long to wait before calling a container unreachable.
///
/// A connection dropped by host policy does not always surface an error to
/// `connect`, in which case the call blocks until the operating system's own
/// timeout. This bound, rather than the error kind, is what makes the probe
/// terminate.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Probe whether this process can reach `ip`.
///
/// Reports what the network stack did, not whether any application is running:
/// a refusal is a healthy answer, and only silence or an explicit
/// unreachable/denied error means the path is broken.
pub async fn probe_ip(ip: &str) -> ContainerProbeResult {
    if ip.is_empty() {
        return ContainerProbeResult::Unknown {
            reason: "the container has no known IP address".to_string(),
        };
    }

    let attempt = tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect((ip, PROBE_PORT))).await;
    match attempt {
        Ok(Err(e)) if e.kind() == io::ErrorKind::ConnectionRefused => {
            debug!("Reachability probe: {ip} refused port {PROBE_PORT}");
            ContainerProbeResult::Reachable {
                detail: format!("{ip} answered on port {PROBE_PORT}"),
            }
        }
        Ok(Ok(_stream)) => ContainerProbeResult::Reachable {
            detail: format!("{ip} accepted port {PROBE_PORT}, which is unusual but reachable"),
        },
        Ok(Err(e)) => ContainerProbeResult::Unreachable {
            error: format!("connecting to {ip} failed: {e}"),
        },
        Err(_) => ContainerProbeResult::Unreachable {
            error: format!("{ip} did not respond within {}s", PROBE_TIMEOUT.as_secs()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_ip_is_unknown_not_unreachable() {
        let result = probe_ip("").await;
        assert!(matches!(result, ContainerProbeResult::Unknown { .. }));
    }

    #[tokio::test]
    async fn refused_counts_as_reachable() {
        // Loopback answers for a port nothing is bound to, which is exactly the
        // signal the probe looks for.
        let result = probe_ip("127.0.0.1").await;
        assert!(
            matches!(result, ContainerProbeResult::Reachable { .. }),
            "a refusal proves the stack answered, got {result:?}"
        );
    }

    #[tokio::test]
    async fn unroutable_address_is_unreachable() {
        // TEST-NET-1: reserved for documentation and not routed anywhere.
        let result = probe_ip("192.0.2.1").await;
        assert!(
            matches!(result, ContainerProbeResult::Unreachable { .. }),
            "an unroutable address must not read as reachable, got {result:?}"
        );
    }
}
