//! Benchmarks for per-request network rule evaluation.
//!
//! `RuleMatcher::evaluate` runs once per HTTP request intercepted by the
//! agent's MITM proxy (`cella-agent/src/mitm.rs`), against every configured
//! rule until one matches. Everything it does per rule — splitting the domain
//! into labels, splitting the path into segments, allocating memo state — is
//! paid on every request, so this bench reports allocation counts alongside
//! wall time.

use cella_network::config::{NetworkConfig, NetworkMode, NetworkRule, RuleAction};
use cella_network::rules::RuleMatcher;

#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

fn main() {
    divan::main();
}

/// Rule-set sizes: a handful of hand-written rules through a policy file an
/// organisation might ship.
const RULE_COUNTS: &[usize] = &[8, 32, 128];

/// Build a rule set shaped like a real policy: mostly domain-only rules with
/// a wildcard label, plus a path-bearing rule every fourth entry.
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
const MISSES: &[(&str, &str)] = &[
    ("registry.npmjs.org", "/@scope/package/-/package-1.0.0.tgz"),
    ("github.com", "/owner/repo/info/refs"),
    ("index.crates.io", "/config.json"),
    ("pypi.org", "/simple/requests/"),
];

/// Requests that match a rule partway through the set.
const HITS: &[(&str, &str)] = &[
    ("host.svc0.internal", "/v1/admin0/delete"),
    ("api5.example.com", "/anything"),
    ("host.svc9.internal", "/health"),
];

#[divan::bench(args = RULE_COUNTS)]
fn evaluate_miss(bencher: divan::Bencher, rules: usize) {
    let matcher = RuleMatcher::new(&build_config(rules));
    bencher.bench(|| {
        for (domain, path) in MISSES {
            divan::black_box(matcher.evaluate(divan::black_box(domain), divan::black_box(path)));
        }
    });
}

#[divan::bench(args = RULE_COUNTS)]
fn evaluate_hit(bencher: divan::Bencher, rules: usize) {
    let matcher = RuleMatcher::new(&build_config(rules));
    bencher.bench(|| {
        for (domain, path) in HITS {
            divan::black_box(matcher.evaluate(divan::black_box(domain), divan::black_box(path)));
        }
    });
}

/// A path rule whose patterns contain no `**`. Positional matching needs
/// neither recursion nor memo state, so this isolates the cost of the
/// `**`-only machinery from the common single-`*` pattern.
#[divan::bench(args = RULE_COUNTS)]
fn evaluate_path_star_only(bencher: divan::Bencher, rules: usize) {
    let mut config = build_config(rules);
    config.rules.push(NetworkRule {
        domain: "star.example.com".to_owned(),
        paths: vec!["/v1/*/items".to_owned(), "/api/*".to_owned()],
        action: RuleAction::Block,
    });
    let matcher = RuleMatcher::new(&config);
    bencher.bench(|| {
        divan::black_box(matcher.evaluate(
            divan::black_box("star.example.com"),
            divan::black_box("/v1/things/items"),
        ));
    });
}

/// Uppercase input forces the domain-lowercasing path that lowercase input
/// can skip — kept separate so the fast path is not averaged with it.
#[divan::bench(args = RULE_COUNTS)]
fn evaluate_mixed_case(bencher: divan::Bencher, rules: usize) {
    let matcher = RuleMatcher::new(&build_config(rules));
    bencher.bench(|| {
        divan::black_box(matcher.evaluate(
            divan::black_box("Registry.NPMJS.org"),
            divan::black_box("/x"),
        ));
    });
}

/// Called per CONNECT to decide whether TLS interception is needed.
#[divan::bench(args = RULE_COUNTS)]
fn needs_path_inspection(bencher: divan::Bencher, rules: usize) {
    let matcher = RuleMatcher::new(&build_config(rules));
    bencher.bench(|| {
        divan::black_box(matcher.domain_needs_path_inspection(divan::black_box("github.com")));
    });
}
