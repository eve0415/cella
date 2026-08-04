//! Standalone driver for profiling [`RuleMatcher::evaluate`] under perf.
//!
//! Same reasoning as `cella-oci/examples/profile_extract.rs`: the divan bench
//! is fine for timing but not for sampling, because perf attributes the whole
//! process and divan's harness is a meaningful share of it. Here the matcher
//! and the request list are built once, and the loop does nothing but
//! evaluate.
//!
//! ```text
//! cargo build --profile profiling --example profile_rules -p cella-network
//! ./target/profiling/examples/profile_rules <rules> <iterations>
//! ```

use std::io::Write as _;

use cella_network::config::{NetworkConfig, NetworkMode, NetworkRule, RuleAction};
use cella_network::rules::RuleMatcher;

fn build_config(rules: usize) -> NetworkConfig {
    let rules = (0..rules)
        .map(|i| {
            let domain = if i % 3 == 0 {
                format!("*.svc{i}.internal")
            } else {
                format!("api{i}.example.com")
            };
            let paths = if i % 4 == 0 {
                vec![format!("/v1/admin{i}/**"), format!("/internal{i}/*")]
            } else {
                Vec::new()
            };
            NetworkRule {
                domain,
                paths,
                action: RuleAction::Block,
            }
        })
        .collect();

    NetworkConfig {
        mode: NetworkMode::Denylist,
        rules,
        ..NetworkConfig::default()
    }
}

/// Requests that fall through every rule — the dominant case for a denylist,
/// and the one that pays the full per-rule cost.
const REQUESTS: &[(&str, &str)] = &[
    ("registry.npmjs.org", "/@scope/package/-/package-1.0.0.tgz"),
    ("github.com", "/owner/repo/info/refs"),
    ("index.crates.io", "/config.json"),
    ("pypi.org", "/simple/requests/"),
];

fn main() {
    let mut args = std::env::args().skip(1);
    let rules: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(128);
    let iterations: usize = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(2_000_000);

    let matcher = RuleMatcher::new(&build_config(rules));

    let start = std::time::Instant::now();
    let mut blocked = 0usize;
    for i in 0..iterations {
        let (domain, path) = REQUESTS[i % REQUESTS.len()];
        if !matcher.evaluate(domain, path).allowed {
            blocked += 1;
        }
    }
    let elapsed = start.elapsed();

    let mut out = std::io::stderr();
    writeln!(
        out,
        "{rules} rules x {iterations} requests: total {elapsed:?}  per-request {:?}  ({blocked} blocked)",
        elapsed / u32::try_from(iterations).unwrap_or(1),
    )
    .ok();
}
