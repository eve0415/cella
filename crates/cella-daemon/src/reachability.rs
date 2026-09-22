//! Host to container reachability probing.
//!
//! Port forwarding fails invisibly when the daemon cannot open a connection to
//! a container: the host-side listener still binds and still accepts, so every
//! forward looks healthy while each connection is dropped the moment the
//! upstream dial fails. This probe answers the one question that binding does
//! not: can this process still reach that container at all?

use std::collections::HashMap;
use std::io;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use cella_protocol::ContainerProbeResult;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::port_manager::ContainerTransport;

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

/// Probe verdicts already reached, keyed by the address block each was
/// reached in.
///
/// A policy that denies this process the container bridge denies the whole
/// interface rather than one address, so a neighbouring container's verdict
/// answers for the block. That matters because registration is a blocking
/// request and response per container: compose brings its services up one at
/// a time, so an unmemoized probe would add its own timeout to every service
/// of every `cella up`. Healthy verdicts are kept for the same reason — a host
/// that works pays for at most one probe per block.
#[derive(Default)]
pub(crate) struct ProbeMemo {
    verdicts: HashMap<String, ContainerProbeResult>,
}

/// A memo shared by the management handlers that read and fill it.
pub(crate) type SharedProbeMemo = Arc<Mutex<ProbeMemo>>;

/// Create an empty shared memo.
pub(crate) fn new_shared_memo() -> SharedProbeMemo {
    Arc::new(Mutex::new(ProbeMemo::default()))
}

impl ProbeMemo {
    /// The verdict already reached for `ip`'s block, if there is one.
    pub(crate) fn recall(&self, ip: &str) -> Option<&ContainerProbeResult> {
        self.verdicts.get(&block_of(ip))
    }

    fn remember(&mut self, ip: &str, verdict: ContainerProbeResult) {
        self.verdicts.insert(block_of(ip), verdict);
    }
}

/// The block a verdict is memoized under: the /24 of an IPv4 container
/// address, since that is the granularity a container bridge is routed at, and
/// the address itself for anything else, whose neighbours the daemon cannot
/// assume to share a fate.
fn block_of(ip: &str) -> String {
    ip.parse::<Ipv4Addr>().map_or_else(
        |_| ip.to_string(),
        |v4| {
            let [a, b, c, _] = v4.octets();
            format!("{a}.{b}.{c}.0/24")
        },
    )
}

/// Probe `ip`'s block at most once for the lifetime of `memo`.
async fn probe_block(
    memo: &Mutex<ProbeMemo>,
    ip: &str,
    probe: impl AsyncFnOnce(&str) -> ContainerProbeResult,
) -> ContainerProbeResult {
    let known = memo.lock().await.recall(ip).cloned();
    if let Some(known) = known {
        debug!("Reachability verdict for {ip} reused from {}", block_of(ip));
        return known;
    }
    let verdict = probe(ip).await;
    memo.lock().await.remember(ip, verdict.clone());
    verdict
}

/// Choose the transport for a container's forwards from what the probe saw.
///
/// Only an explicit unreachable verdict moves a container onto the tunnel. The
/// probe dials a port the workspace does not use, so a filter that rejects
/// only that port is indistinguishable from silence: `Unknown` means the
/// question was not answered, and an unanswered question is not grounds for
/// taking every forward off a path that works.
const fn select_transport(
    runtime_uses_direct_ip: bool,
    probed: Option<&ContainerProbeResult>,
) -> ContainerTransport {
    if !runtime_uses_direct_ip {
        return ContainerTransport::Tunnel;
    }
    match probed {
        Some(ContainerProbeResult::Unreachable { .. }) => ContainerTransport::Tunnel,
        _ => ContainerTransport::Direct,
    }
}

/// Decide how a container's forwards will be reached, probing its address
/// where the runtime claims the daemon can use it.
///
/// The claim is what needs testing: the runtime rule says container addresses
/// are routable on this host, which is not the same as this process being
/// permitted to use them.
pub(crate) async fn decide_transport(
    memo: &Mutex<ProbeMemo>,
    runtime_uses_direct_ip: bool,
    container_ip: Option<&str>,
    probe: impl AsyncFnOnce(&str) -> ContainerProbeResult,
) -> ContainerTransport {
    if !runtime_uses_direct_ip {
        return ContainerTransport::Tunnel;
    }

    // Nothing to probe yet. A forward that needs the address is skipped until
    // one is known, so the direct path stays the assumption rather than a
    // verdict drawn from a missing address.
    let Some(ip) = container_ip.filter(|ip| !ip.is_empty()) else {
        return select_transport(true, None);
    };

    let probed = probe_block(memo, ip, probe).await;
    let transport = select_transport(true, Some(&probed));
    if transport == ContainerTransport::Tunnel {
        warn!(
            "Cannot reach {ip} from this process, so forwards for it will use the agent tunnel \
             instead of its address. Run `cella doctor` for what was observed."
        );
    }
    transport
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

    fn reachable() -> ContainerProbeResult {
        ContainerProbeResult::Reachable {
            detail: "answered".to_string(),
        }
    }

    #[test]
    fn only_an_unreachable_verdict_moves_a_container_to_the_tunnel() {
        assert_eq!(
            select_transport(
                true,
                Some(&ContainerProbeResult::Unreachable {
                    error: "no route to host".to_string(),
                })
            ),
            ContainerTransport::Tunnel
        );

        for inconclusive in [
            reachable(),
            ContainerProbeResult::Unknown {
                reason: "did not answer".to_string(),
            },
            ContainerProbeResult::NotApplicable {
                reason: "nothing to test".to_string(),
            },
        ] {
            assert_eq!(
                select_transport(true, Some(&inconclusive)),
                ContainerTransport::Direct,
                "an unanswered probe must not take forwards off a working path: {inconclusive:?}"
            );
        }

        assert_eq!(
            select_transport(true, None),
            ContainerTransport::Direct,
            "no address to probe is not evidence against the direct path"
        );
    }

    #[test]
    fn a_runtime_without_direct_addressing_is_always_tunnelled() {
        for probed in [
            None,
            Some(reachable()),
            Some(ContainerProbeResult::Unreachable {
                error: "no route to host".to_string(),
            }),
        ] {
            assert_eq!(
                select_transport(false, probed.as_ref()),
                ContainerTransport::Tunnel,
                "the runtime has no direct path to choose, whatever a probe saw"
            );
        }
    }

    #[tokio::test]
    async fn a_block_is_probed_once_however_many_containers_it_holds() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let memo = Mutex::new(ProbeMemo::default());
        let probes = AtomicUsize::new(0);
        let count = async |_: &str| {
            probes.fetch_add(1, Ordering::SeqCst);
            reachable()
        };

        for ip in ["172.20.0.5", "172.20.0.9", "172.20.0.5"] {
            probe_block(&memo, ip, count).await;
        }
        assert_eq!(
            probes.load(Ordering::SeqCst),
            1,
            "neighbours in one /24 share a verdict"
        );

        probe_block(&memo, "172.21.0.5", count).await;
        assert_eq!(
            probes.load(Ordering::SeqCst),
            2,
            "another bridge is another question"
        );
    }

    #[tokio::test]
    async fn a_healthy_verdict_is_memoized_too() {
        // The cost of a working host has to be one probe per block, not one
        // per container, or every compose service pays the probe timeout.
        let memo = Mutex::new(ProbeMemo::default());
        probe_block(&memo, "172.20.0.5", async |_| reachable()).await;
        assert!(memo.lock().await.recall("172.20.0.7").is_some());
    }

    #[tokio::test]
    async fn no_direct_addressing_means_no_probe() {
        let memo = Mutex::new(ProbeMemo::default());
        let transport = decide_transport(&memo, false, Some("172.20.0.5"), async |_| {
            panic!("a tunnelled runtime has nothing to probe")
        })
        .await;
        assert_eq!(transport, ContainerTransport::Tunnel);
    }

    #[tokio::test]
    async fn an_unreachable_address_decides_the_tunnel_for_its_block() {
        let memo = Mutex::new(ProbeMemo::default());
        let unreachable = async |ip: &str| ContainerProbeResult::Unreachable {
            error: format!("connecting to {ip} failed"),
        };

        assert_eq!(
            decide_transport(&memo, true, Some("172.20.0.5"), unreachable).await,
            ContainerTransport::Tunnel
        );
        assert_eq!(
            decide_transport(&memo, true, Some("172.20.0.9"), async |_| panic!(
                "the block's verdict is already known"
            ))
            .await,
            ContainerTransport::Tunnel
        );
    }

    #[tokio::test]
    async fn an_unknown_address_keeps_the_direct_path_unprobed() {
        let memo = Mutex::new(ProbeMemo::default());
        for ip in [None, Some("")] {
            let transport = decide_transport(&memo, true, ip, async |_| {
                panic!("there is no address to probe")
            })
            .await;
            assert_eq!(transport, ContainerTransport::Direct);
        }
    }

    #[test]
    fn a_block_is_the_first_three_octets_of_an_ipv4_address() {
        assert_eq!(block_of("172.20.0.5"), block_of("172.20.0.250"));
        assert_ne!(block_of("172.20.0.5"), block_of("172.20.1.5"));
        // Anything that is not an IPv4 address answers only for itself.
        assert_eq!(block_of("fd00::5"), "fd00::5");
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
