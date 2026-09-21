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

    // Silence is not proof. A filter that drops this port while permitting the
    // ones the workspace actually uses looks identical from here, so an
    // unanswered probe is inconclusive rather than a fault.
    let Ok(attempt) =
        tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect((ip, PROBE_PORT))).await
    else {
        return ContainerProbeResult::Unknown {
            reason: format!(
                "{ip} did not answer on port {PROBE_PORT} within {}s, which a filtered port \
                 also looks like",
                PROBE_TIMEOUT.as_secs()
            ),
        };
    };

    match attempt {
        Ok(_stream) => ContainerProbeResult::Reachable {
            detail: format!("{ip} accepted port {PROBE_PORT}, which is unusual but reachable"),
        },
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            debug!("Reachability probe: {ip} refused port {PROBE_PORT}");
            ContainerProbeResult::Reachable {
                detail: format!("{ip} answered on port {PROBE_PORT}"),
            }
        }
        Err(e) if is_unreachable(e.kind()) => ContainerProbeResult::Unreachable {
            error: format!("connecting to {ip} failed: {e}"),
        },
        Err(e) => ContainerProbeResult::Unknown {
            reason: format!("probing {ip} was inconclusive: {e}"),
        },
    }
}

/// Whether an error kind means the network path itself is unusable, rather
/// than the service behind it being absent.
pub(crate) const fn is_unreachable(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::PermissionDenied
    )
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

    #[test]
    fn only_path_level_errors_count_as_unreachable() {
        assert!(is_unreachable(io::ErrorKind::HostUnreachable));
        assert!(is_unreachable(io::ErrorKind::NetworkUnreachable));
        assert!(is_unreachable(io::ErrorKind::PermissionDenied));
        // A refusal proves the stack answered, and a reset says nothing about
        // whether the path works.
        assert!(!is_unreachable(io::ErrorKind::ConnectionRefused));
        assert!(!is_unreachable(io::ErrorKind::ConnectionReset));
    }
}
