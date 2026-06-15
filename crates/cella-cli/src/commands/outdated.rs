//! `cella outdated` — show current and available versions for configured features.
//!
//! Mirrors the official `@devcontainers/cli outdated` command. For every
//! versionable OCI feature reference in the config, it emits the five fields
//! the official CLI computes in `loadVersionInfo`:
//!
//! ```text
//! { current, wanted, wantedMajor, latest, latestMajor }
//! ```
//!
//! - `latest` is the newest strict-semver published tag.
//! - `wanted` is the highest published tag satisfying the reference's tag
//!   constraint (node-semver semantics, see [`find_wanted`]).
//! - `current` is the lockfile-pinned version if present, else `wanted`.
//! - `*Major` are the major components of `wanted` / `latest`.
//!
//! Non-versionable references (local paths, tarball URLs, deprecated bare
//! identifiers) are filtered out, matching the official `getRef` returning
//! `undefined` for them. Output preserves config declaration order.

use std::path::PathBuf;

use clap::Args;
use semver::Version;
use serde::Serialize;

use crate::commands::OutputFormat;
use crate::commands::features::resolve::{self, CommonFeatureFlags};

/// Show current and available versions.
#[derive(Args)]
pub struct OutdatedArgs {
    #[command(flatten)]
    pub common: CommonFeatureFlags,

    /// Output format.
    ///
    /// `--output-format` is accepted as an alias for parity with the official
    /// `@devcontainers/cli`.
    #[arg(
        long,
        visible_alias = "output-format",
        value_enum,
        default_value = "text"
    )]
    pub output: OutputFormat,

    /// Accepted for CLI parity; has no effect on the emitted report.
    #[arg(long, hide = true)]
    pub check: bool,

    /// Number of columns to render output for (compatibility no-op). Paired with
    /// `--terminal-rows`, matching the official's `implies: ['terminal-rows']`.
    #[arg(long, requires = "terminal_rows")]
    pub terminal_columns: Option<u16>,

    /// Number of rows to render output for (compatibility no-op). Paired with
    /// `--terminal-columns`, matching the official's `implies`.
    #[arg(long, requires = "terminal_columns")]
    pub terminal_rows: Option<u16>,

    /// Accepted for parity; the official `outdated` doesn't actually expose this
    /// (kept hidden as a no-op so scripts passing it don't break).
    #[arg(long, hide = true)]
    pub user_data_folder: Option<PathBuf>,

    /// Log verbosity (seeded into the global tracing filter by `main.rs`).
    #[arg(long = "log-level", value_enum)]
    pub log_level: Option<super::LogLevel>,

    /// Log output format (seeded into the tracing subscriber by `main.rs`).
    #[arg(long = "log-format", value_enum, default_value = "text")]
    pub log_format: super::LogFormat,
}

impl OutdatedArgs {
    /// Execute the `outdated` command.
    ///
    /// # Errors
    ///
    /// Returns an error on config discovery or parse failure. Per-feature
    /// registry failures do not abort: the feature still appears with its
    /// version fields omitted, matching the official CLI.
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let config_path = resolve::discover_config(&self.common)?;
        let raw = resolve::read_raw_config(&config_path)?;
        let stripped = cella_jsonc::strip(&raw)?;
        let config: serde_json::Value = serde_json::from_str(&stripped)?;
        // `extract_features` loses source order (serde_json sorts object keys
        // without the `preserve_order` feature). The official CLI emits features
        // in config declaration order, so re-sort by the order they appear in
        // the raw JSONC.
        let features = order_features_by_source(&raw, resolve::extract_features(&config));

        // Lockfile is keyed by the full userFeatureId (the reference as written
        // in the config), matching the official `lockfile.features[id]` lookup.
        // A corrupt/unreadable lockfile is treated as absent (best-effort) but
        // logged, so a malformed lockfile is still diagnosable.
        let lockfile = match cella_features::read_lockfile(&config_path) {
            Ok(lockfile) => lockfile,
            Err(err) => {
                tracing::warn!("outdated: ignoring unreadable lockfile: {err}");
                None
            }
        };

        let report = load_version_info(&features, lockfile.as_ref()).await;

        match self.output.resolve() {
            OutputFormat::Text => print_text(&report),
            // `Auto` is collapsed to `Text` / `Json` by `resolve()`.
            OutputFormat::Auto | OutputFormat::Json => print_json(&report)?,
        }

        Ok(())
    }
}

/// Reorder `features` to follow the order their keys appear in the raw JSONC
/// source, mirroring the official CLI's "reorder to match config declaration
/// order".
///
/// Keys present in the source are emitted in source order; any key not found
/// in the source (should not happen, but defensive) keeps its original relative
/// position at the end.
fn order_features_by_source(
    raw: &str,
    features: Vec<(String, serde_json::Value)>,
) -> Vec<(String, serde_json::Value)> {
    let source_order = features_in_source_order(raw);
    if source_order.is_empty() {
        return features;
    }

    // Rank each key by its source position once (O(n)); sorting is then
    // O(n log n) rather than an O(n) scan per comparison.
    let rank: std::collections::HashMap<&str, usize> = source_order
        .iter()
        .enumerate()
        .map(|(i, k)| (k.as_str(), i))
        .collect();
    let mut ordered = features;
    // Stable sort keeps the original order among any keys missing from the
    // source (all ranked `usize::MAX`).
    ordered.sort_by_key(|(key, _)| rank.get(key.as_str()).copied().unwrap_or(usize::MAX));
    ordered
}

/// Extract the `features` object's keys in the order they appear in the raw
/// JSONC source, using the comment-preserving CST parser.
///
/// Returns an empty vector if the source cannot be parsed or has no `features`
/// object; callers then fall back to the input order.
fn features_in_source_order(raw: &str) -> Vec<String> {
    use jsonc_parser::ParseOptions;
    use jsonc_parser::cst::CstRootNode;

    let Ok(root) = CstRootNode::parse(raw, &ParseOptions::default()) else {
        return Vec::new();
    };
    let Some(features) = root
        .object_value()
        .and_then(|obj| obj.get("features"))
        .and_then(|prop| prop.object_value())
    else {
        return Vec::new();
    };

    features
        .properties()
        .into_iter()
        .filter_map(|prop| prop.name().and_then(|name| name.decoded_value().ok()))
        .collect()
}

/// The five-field version record the official CLI emits per feature.
///
/// `JSON.stringify` omits `undefined` fields; we mirror that with
/// `skip_serializing_if = "Option::is_none"` so absent fields never serialize
/// as `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wanted: Option<String>,
    #[serde(rename = "wantedMajor", skip_serializing_if = "Option::is_none")]
    pub wanted_major: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    #[serde(rename = "latestMajor", skip_serializing_if = "Option::is_none")]
    pub latest_major: Option<String>,
}

/// The per-feature report, preserving config declaration order.
///
/// Stored as an ordered list of `(userFeatureId, info)` pairs so order is
/// preserved without depending on `serde_json`'s `preserve_order` feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutdatedReport {
    entries: Vec<(String, VersionInfo)>,
}

impl Serialize for OutdatedReport {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap as _;
        let mut features = serializer.serialize_map(Some(self.entries.len()))?;
        for (key, info) in &self.entries {
            features.serialize_entry(key, info)?;
        }
        features.end()
    }
}

/// Serialization wrapper that nests the report under a `features` key, yielding
/// the official top-level shape `{ "features": { … } }`.
struct OutdatedReportWrapper<'a>(&'a OutdatedReport);

impl Serialize for OutdatedReportWrapper<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct as _;
        let mut s = serializer.serialize_struct("OutdatedReport", 1)?;
        s.serialize_field("features", self.0)?;
        s.end()
    }
}

/// Build the per-feature version report (the Rust analogue of the official
/// `loadVersionInfo`).
///
/// Iterates config features in declaration order, skips non-versionable
/// references, fetches and filters published tags, and computes the five
/// version fields.
///
/// A per-feature registry failure does NOT abort the command: it is treated as
/// an empty version list, so the feature still appears with its tag-derived
/// fields omitted (only `current` survives, and only if a lockfile pins it).
/// This matches the official `getVersionsStrictSorted` returning `undefined`,
/// which `loadVersionInfo` folds into `[]` while still recording the feature.
async fn load_version_info(
    features: &[(String, serde_json::Value)],
    lockfile: Option<&cella_features::Lockfile>,
) -> OutdatedReport {
    let mut entries: Vec<(String, VersionInfo)> = Vec::new();

    for (user_feature_id, _options) in features {
        let Some((registry, repository, tag)) = versionable_oci_ref(user_feature_id) else {
            // Local paths, tarball URLs, deprecated bare ids, parse errors:
            // `getRef` returns undefined → feature is filtered out entirely.
            continue;
        };

        let name = format!("{registry}/{repository}");
        let versions = match cella_oci::fetch_published_tags(&name).await {
            Ok(published) => strict_semver_descending(&published),
            Err(err) => {
                // Degrade per-feature: the entry is still emitted, just empty.
                tracing::debug!("outdated: failed to list tags for {name}: {err}");
                Vec::new()
            }
        };

        // Only a full-semver lockfile pin is meaningful for `current`. cella's
        // lockfile currently stores the OCI *tag* (e.g. `"2"`), not the resolved
        // feature version (`"2.12.2"`) the official records — so emitting it
        // would make `current` echo the range while `wanted`/`latest` are
        // concrete. Fall back to the resolved `wanted` until the lockfile schema
        // stores the full version (tracked follow-up).
        let lockfile_version = lockfile
            .and_then(|lf| lf.features.get(user_feature_id))
            .map(|entry| entry.version.clone())
            .filter(|v| Version::parse(v).is_ok());

        // `wanted` comes purely from the tag (cella refs always carry one).
        // It is NOT backfilled from the lockfile when the tag matches nothing:
        // the official code assigns `wanted = versions.find(...)` (possibly
        // undefined), so a no-match tag leaves `wanted`/`wantedMajor` omitted.
        // The lockfile version only feeds `current`.
        let wanted = find_wanted(&versions, &tag).map(ToString::to_string);
        let latest = versions.first().map(ToString::to_string);

        let info = VersionInfo {
            current: lockfile_version.or_else(|| wanted.clone()),
            wanted_major: wanted.as_deref().and_then(major_string),
            latest_major: latest.as_deref().and_then(major_string),
            wanted,
            latest,
        };

        entries.push((user_feature_id.clone(), info));
    }

    OutdatedReport { entries }
}

/// Resolve a user feature id to a versionable `(registry, repository, tag)`, or
/// `None` if it is not a versionable OCI reference.
///
/// Returns `None` for local paths, tarball URLs, deprecated bare identifiers,
/// and any reference that fails to parse — matching the official `getRef`
/// returning `undefined`. The tag defaults to `"latest"` when absent.
fn versionable_oci_ref(user_feature_id: &str) -> Option<(String, String, String)> {
    use cella_features::{FeatureRef, NormalizedRef};

    // Digest-pinned refs (`…@sha256:…`) aren't tag-versionable, and cella's
    // `FeatureRef` parser mis-splits the digest colon as a tag — which would
    // list tags for the wrong repository and emit a misleading entry. Skip them.
    // (Full digest support — `latest` from the repo plus the digest-resolved
    // pinned version, as the official does — is a follow-up.)
    if user_feature_id.contains("@sha256:") {
        return None;
    }

    let parsed = FeatureRef::parse(user_feature_id).ok()?;
    // Only OCI / GitHub-shorthand references are versionable. Local paths,
    // tarball URLs, and deprecated bare identifiers are filtered out.
    match parsed {
        FeatureRef::Oci { .. } | FeatureRef::GitHubShorthand { .. } => {}
        FeatureRef::TarballUrl { .. }
        | FeatureRef::LocalPath { .. }
        | FeatureRef::Deprecated { .. } => return None,
    }

    // `normalize` needs a workspace root only to resolve local paths, which we
    // have already excluded; any path works for the remaining OCI arms.
    let (normalized, _warning) = parsed.normalize(std::path::Path::new(".")).ok()?;
    match normalized {
        NormalizedRef::OciTarget {
            registry,
            repository,
            tag,
        } => Some((registry, repository, tag)),
        NormalizedRef::HttpTarget { .. } | NormalizedRef::LocalTarget { .. } => None,
    }
}

/// Filter published tags to strict-valid full semver and sort DESCENDING
/// (newest first), so index 0 is the latest version.
///
/// Mirrors the official `getVersionsStrictSorted` (filter `semver.valid`, sort
/// ascending) followed by `loadVersionInfo`'s `.reverse()`. Bare-major (`1`),
/// minor (`1.2`), and named (`latest`) tags are dropped because they are not
/// strict full semver.
fn strict_semver_descending(published: &[String]) -> Vec<Version> {
    let mut versions: Vec<Version> = published
        .iter()
        .filter_map(|t| Version::parse(t).ok())
        .collect();
    // Ascending, then reverse → descending. `Version` orders by precedence
    // (major, minor, patch, then pre-release), matching node-semver's compare.
    versions.sort();
    versions.reverse();
    versions
}

/// Find the highest version in a DESCENDING list that satisfies `tag` — the
/// `wanted` version.
///
/// `versions` must already be sorted descending (newest first), so the first
/// match is the highest satisfying version. Mirrors the official `wanted`
/// computation on the tag path:
/// - `latest` → the newest version (`versions[0]`).
/// - otherwise → the first version satisfying the tag under node-semver range
///   semantics ([`satisfies`]).
///
/// Returns `None` when no published version satisfies the tag. The official
/// code likewise leaves `wanted` undefined in that case — it is *not* backfilled
/// from the lockfile (only `current` reads the lockfile).
fn find_wanted<'a>(versions: &'a [Version], tag: &str) -> Option<&'a Version> {
    if tag == "latest" {
        return versions.first();
    }
    versions.iter().find(|v| satisfies(v, tag))
}

/// node-semver `satisfies(version, range)` for the tag forms an OCI tag can
/// take.
///
/// OCI tags only contain `[A-Za-z0-9._-]` (no `^ ~ > <`), so the only range
/// shapes are `latest`, `N`, `N.M`, and a full `N.M.P[-pre]`. node-semver
/// treats a partial version as an implicit range that **excludes
/// pre-releases**:
/// - `N` → `>=N.0.0 <(N+1).0.0`, stable only.
/// - `N.M` → `>=N.M.0 <N.(M+1).0`, stable only.
/// - `N.M.P[-pre]` (a complete version) → exact equality.
///
/// Note: the Rust `semver::VersionReq` parser treats `"1.2"` as `^1.2`
/// (`>=1.2.0 <2.0.0`), which diverges from node-semver — so range membership is
/// implemented explicitly here rather than via `VersionReq`.
fn satisfies(version: &Version, tag: &str) -> bool {
    // A complete `N.M.P[-pre]` tag means exact-version match.
    if let Ok(exact) = Version::parse(tag) {
        return *version == exact;
    }

    let mut parts = tag.split('.');
    let major = parts.next().and_then(|p| p.parse::<u64>().ok());
    let minor = parts.next().and_then(|p| p.parse::<u64>().ok());
    // Any extra component means this is not a partial-version range we
    // recognize → no match.
    if parts.next().is_some() {
        return false;
    }

    match (major, minor) {
        // `N.M` → stable versions with matching major and minor.
        (Some(maj), Some(min)) => {
            version.major == maj && version.minor == min && version.pre.is_empty()
        }
        // `N` → stable versions with matching major.
        (Some(maj), None) => version.major == maj && version.pre.is_empty(),
        // Unparseable leading component → no match.
        _ => false,
    }
}

/// Extract the major component of a version string as a decimal string.
///
/// Returns `None` if the string is not strict semver (values from our filtered
/// version list always are, but `current` may come from a lockfile and is
/// treated defensively).
fn major_string(version: &str) -> Option<String> {
    Version::parse(version).ok().map(|v| v.major.to_string())
}

/// Render the 4-column text table (`Feature`, `Current`, `Wanted`, `Latest`) to
/// stdout, mirroring the official `text-table` output. Absent fields render as
/// `-`.
fn print_text(report: &OutdatedReport) {
    let rows = text_rows(report);
    let widths = column_widths(&rows);
    let mut out = String::new();
    for row in &rows {
        out.push_str(&format_row(row, &widths));
        out.push('\n');
    }
    print!("{out}");
}

/// Build the table rows (header + one per feature) with `-` for absent fields.
fn text_rows(report: &OutdatedReport) -> Vec<[String; 4]> {
    let dash = || "-".to_owned();
    let mut rows: Vec<[String; 4]> = Vec::with_capacity(report.entries.len() + 1);
    rows.push([
        "Feature".to_owned(),
        "Current".to_owned(),
        "Wanted".to_owned(),
        "Latest".to_owned(),
    ]);
    for (key, info) in &report.entries {
        rows.push([
            cella_features::feature_id_without_version(key).to_owned(),
            info.current.clone().unwrap_or_else(dash),
            info.wanted.clone().unwrap_or_else(dash),
            info.latest.clone().unwrap_or_else(dash),
        ]);
    }
    rows
}

/// Compute the maximum display width of each of the 4 columns.
fn column_widths(rows: &[[String; 4]]) -> [usize; 4] {
    let mut widths = [0_usize; 4];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    widths
}

/// Format a single row, left-aligning and padding each column to its width.
///
/// `text-table` separates columns with two spaces and never emits trailing
/// padding, so the rendered line is right-trimmed.
fn format_row(row: &[String; 4], widths: &[usize; 4]) -> String {
    let mut line = String::new();
    for (i, cell) in row.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        line.push_str(cell);
        if i + 1 < row.len() {
            let pad = widths[i].saturating_sub(cell.chars().count());
            for _ in 0..pad {
                line.push(' ');
            }
        }
    }
    line.trim_end().to_owned()
}

/// Serialize the report as `{ "features": { … } }` to stdout.
///
/// Pretty-prints (2-space indent) when stdout is a TTY, compact otherwise,
/// matching the official `JSON.stringify(outdated, undefined, isTTY ? '  ' :
/// undefined)`.
fn print_json(report: &OutdatedReport) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::io::IsTerminal as _;
    let wrapper = OutdatedReportWrapper(report);
    let text = if std::io::stdout().is_terminal() {
        serde_json::to_string_pretty(&wrapper)?
    } else {
        serde_json::to_string(&wrapper)?
    };
    println!("{text}");
    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use semver::Version;

    use super::*;

    /// Build a DESCENDING version list, as the production code passes around.
    fn versions(list: &[&str]) -> Vec<Version> {
        strict_semver_descending(&list.iter().map(ToString::to_string).collect::<Vec<_>>())
    }

    #[test]
    fn accepts_official_config_and_log_flags() {
        use clap::Parser as _;
        let cli = crate::Cli::try_parse_from([
            "cella",
            "outdated",
            "--config",
            "/x/devcontainer.json",
            "--log-level",
            "debug",
            "--log-format",
            "json",
            "--terminal-columns",
            "80",
            "--terminal-rows",
            "40",
        ])
        .expect("official outdated flags must parse");
        let crate::commands::Command::Outdated(args) = &cli.command else {
            panic!("expected outdated subcommand");
        };
        assert_eq!(
            args.common.file.as_deref(),
            Some(std::path::Path::new("/x/devcontainer.json")),
            "--config must populate the config path (alias of --file)"
        );
        assert!(matches!(
            args.log_level,
            Some(super::super::LogLevel::Debug)
        ));
        assert!(matches!(args.log_format, super::super::LogFormat::Json));
    }

    // -----------------------------------------------------------------------
    // strict_semver_descending
    // -----------------------------------------------------------------------

    #[test]
    fn strict_filter_drops_major_minor_and_latest() {
        let input = ["1", "1.2", "latest", "1.2.3", "2.0.0", "1.10.0"];
        let v =
            strict_semver_descending(&input.iter().map(ToString::to_string).collect::<Vec<_>>());
        let as_str: Vec<String> = v.iter().map(ToString::to_string).collect();
        // Only full semver kept, sorted descending (2.0.0 > 1.10.0 > 1.2.3).
        assert_eq!(as_str, vec!["2.0.0", "1.10.0", "1.2.3"]);
    }

    #[test]
    fn descending_orders_prerelease_below_release() {
        let v = versions(&["1.0.0", "1.0.0-alpha", "1.0.1"]);
        let as_str: Vec<String> = v.iter().map(ToString::to_string).collect();
        // 1.0.1 > 1.0.0 > 1.0.0-alpha (a pre-release sorts below its release).
        assert_eq!(as_str, vec!["1.0.1", "1.0.0", "1.0.0-alpha"]);
    }

    // -----------------------------------------------------------------------
    // satisfies / find_wanted
    // -----------------------------------------------------------------------

    #[test]
    fn tag_major_picks_highest_stable_excluding_next_major_and_prerelease() {
        let v = versions(&["1.0.0", "1.5.0", "1.5.1", "1.6.0-rc.1", "2.0.0"]);
        // "1" → highest 1.x stable: 1.5.1 (not 2.0.0, not 1.6.0-rc.1).
        let w = find_wanted(&v, "1").map(ToString::to_string);
        assert_eq!(w, Some("1.5.1".to_owned()));
    }

    #[test]
    fn tag_major_excludes_prerelease_of_same_major() {
        let v = versions(&["1.0.0-alpha", "1.0.0-beta"]);
        // No stable 1.x → no match (ranges exclude pre-releases).
        assert!(find_wanted(&v, "1").is_none());
    }

    #[test]
    fn tag_minor_picks_highest_matching_minor_not_next_minor() {
        let v = versions(&["1.2.0", "1.2.9", "1.3.0", "1.2.10"]);
        // "1.2" → highest 1.2.x: 1.2.10 (not 1.3.0).
        let w = find_wanted(&v, "1.2").map(ToString::to_string);
        assert_eq!(w, Some("1.2.10".to_owned()));
    }

    #[test]
    fn tag_minor_excludes_prerelease() {
        let v = versions(&["1.2.0", "1.2.5-rc.1"]);
        let w = find_wanted(&v, "1.2").map(ToString::to_string);
        assert_eq!(w, Some("1.2.0".to_owned()));
    }

    #[test]
    fn tag_exact_full_version_matches_only_itself() {
        let v = versions(&["1.2.2", "1.2.3", "1.2.4"]);
        let w = find_wanted(&v, "1.2.3").map(ToString::to_string);
        assert_eq!(w, Some("1.2.3".to_owned()));
    }

    #[test]
    fn tag_exact_full_version_no_match_returns_none() {
        let v = versions(&["1.2.2", "1.2.4"]);
        assert!(find_wanted(&v, "1.2.3").is_none());
    }

    #[test]
    fn tag_exact_prerelease_matches_exactly() {
        let v = versions(&["1.0.0", "1.0.0-rc.1"]);
        let w = find_wanted(&v, "1.0.0-rc.1").map(ToString::to_string);
        assert_eq!(w, Some("1.0.0-rc.1".to_owned()));
    }

    #[test]
    fn tag_latest_picks_versions_first() {
        let v = versions(&["1.0.0", "2.0.0", "1.5.0"]);
        let w = find_wanted(&v, "latest").map(ToString::to_string);
        // Descending list head is the newest.
        assert_eq!(w, Some("2.0.0".to_owned()));
    }

    #[test]
    fn tag_no_match_returns_none() {
        let v = versions(&["1.0.0", "1.1.0"]);
        // "3" has no 3.x stable.
        assert!(find_wanted(&v, "3").is_none());
    }

    #[test]
    fn satisfies_rust_caret_divergence_guarded() {
        // node-semver: "1.2" excludes 1.3.0. Rust VersionReq("1.2") == ^1.2
        // would INCLUDE 1.3.0 — we must not.
        let v13 = Version::parse("1.3.0").unwrap();
        assert!(!satisfies(&v13, "1.2"));
        let v12 = Version::parse("1.2.9").unwrap();
        assert!(satisfies(&v12, "1.2"));
        // "1" must exclude 2.0.0.
        let v20 = Version::parse("2.0.0").unwrap();
        assert!(!satisfies(&v20, "1"));
    }

    // -----------------------------------------------------------------------
    // wanted is pure-tag: the lockfile NEVER backfills wanted (only current)
    // -----------------------------------------------------------------------

    #[test]
    fn wanted_ignores_lockfile_when_tag_matches() {
        // Tag "1" → 1.5.0 regardless of any lockfile pin: `wanted` is tag-only.
        let v = versions(&["1.0.0", "1.5.0", "2.0.0"]);
        let w = find_wanted(&v, "1").map(ToString::to_string);
        assert_eq!(w, Some("1.5.0".to_owned()));
    }

    #[test]
    fn wanted_is_none_when_tag_matches_nothing_even_with_lockfile() {
        // The official code assigns `wanted = versions.find(...)` (undefined on
        // no match) — it is NOT backfilled from the lockfile. Build the full
        // entry via the lockfile path to assert current/wanted split.
        let lf = lockfile_pinning("ghcr.io/x/y:99", "0.9.0");
        let rt = tokio::runtime::Runtime::new().unwrap();
        // `registry.invalid` never resolves → empty version list → no tag match.
        let features = vec![("registry.invalid/x/y:99".to_owned(), serde_json::json!({}))];
        let report = rt.block_on(load_version_info(&features, Some(&lf)));
        // Different id than the lockfile entry → no lockfile current here, but
        // the point is the empty-versions/no-match path: wanted must be None.
        let (_key, info) = &report.entries[0];
        assert_eq!(info.wanted, None, "no tag match → wanted omitted");
        assert_eq!(info.wanted_major, None);
    }

    #[test]
    fn current_uses_lockfile_while_wanted_stays_none_on_no_match() {
        // node:99 (no such version) with a lockfile pinning that exact id to
        // 1.0.0: official emits current=1.0.0, wanted/wantedMajor omitted.
        let lf = lockfile_pinning("registry.invalid/x/y:99", "1.0.0");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let features = vec![("registry.invalid/x/y:99".to_owned(), serde_json::json!({}))];
        let report = rt.block_on(load_version_info(&features, Some(&lf)));
        let (_key, info) = &report.entries[0];
        assert_eq!(info.current, Some("1.0.0".to_owned()), "current = lockfile");
        assert_eq!(info.wanted, None, "wanted is NOT backfilled from lockfile");
        assert_eq!(info.wanted_major, None);
        // Serialized shape matches the official `{current}`-only object.
        let json = serde_json::to_string(info).unwrap();
        assert_eq!(json, r#"{"current":"1.0.0"}"#);
    }

    /// Build a single-entry lockfile pinning `key` to `version`.
    fn lockfile_pinning(key: &str, version: &str) -> cella_features::Lockfile {
        let mut features = std::collections::BTreeMap::new();
        features.insert(
            key.to_owned(),
            cella_features::LockfileEntry {
                version: version.to_owned(),
                resolved: format!("{key}@sha256:{}", "0".repeat(64)),
                integrity: format!("sha256:{}", "0".repeat(64)),
                depends_on: vec![],
            },
        );
        cella_features::Lockfile { features }
    }

    // -----------------------------------------------------------------------
    // versionable_oci_ref (filtering)
    // -----------------------------------------------------------------------

    #[test]
    fn versionable_oci_ref_accepts_oci_with_tag() {
        let r = versionable_oci_ref("ghcr.io/devcontainers/features/node:1");
        assert_eq!(
            r,
            Some((
                "ghcr.io".to_owned(),
                "devcontainers/features/node".to_owned(),
                "1".to_owned()
            ))
        );
    }

    #[test]
    fn versionable_oci_ref_defaults_tag_to_latest() {
        let r = versionable_oci_ref("ghcr.io/devcontainers/features/node");
        assert_eq!(
            r,
            Some((
                "ghcr.io".to_owned(),
                "devcontainers/features/node".to_owned(),
                "latest".to_owned()
            ))
        );
    }

    #[test]
    fn versionable_oci_ref_skips_local_path() {
        assert!(versionable_oci_ref("./my-feature").is_none());
        assert!(versionable_oci_ref("../shared/feat").is_none());
    }

    #[test]
    fn versionable_oci_ref_skips_tarball_url() {
        assert!(versionable_oci_ref("https://example.com/feat.tgz").is_none());
    }

    #[test]
    fn versionable_oci_ref_skips_deprecated_bare_id() {
        // `fish` is a deprecated bare identifier → filtered out.
        assert!(versionable_oci_ref("fish").is_none());
    }

    #[test]
    fn versionable_oci_ref_skips_digest_pinned() {
        // Digest-pinned refs aren't tag-versionable; skip rather than list tags
        // for the wrong repository (cella's parser mis-splits the digest colon).
        let digest = format!(
            "ghcr.io/devcontainers/features/node@sha256:{}",
            "a".repeat(64)
        );
        assert!(versionable_oci_ref(&digest).is_none());
    }

    #[test]
    fn tag_only_lockfile_version_is_ignored_for_current() {
        // cella's lockfile stores the OCI tag (`"2"`), not a full version. The
        // guard rejects it so `current` doesn't echo the range; with no tag
        // match (empty versions) `current` stays None rather than `"2"`.
        let lf = lockfile_pinning("registry.invalid/x/y:99", "2");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let features = vec![("registry.invalid/x/y:99".to_owned(), serde_json::json!({}))];
        let report = rt.block_on(load_version_info(&features, Some(&lf)));
        let (_key, info) = &report.entries[0];
        assert_eq!(
            info.current, None,
            "tag-only lockfile version must be ignored"
        );
    }

    // -----------------------------------------------------------------------
    // JSON shape
    // -----------------------------------------------------------------------

    #[test]
    fn empty_report_serializes_to_empty_features() {
        let report = OutdatedReport { entries: vec![] };
        let json = serde_json::to_string(&OutdatedReportWrapper(&report)).unwrap();
        assert_eq!(json, r#"{"features":{}}"#);
    }

    #[test]
    fn version_info_omits_none_fields() {
        // Only latest / latestMajor present; the rest must be omitted, not null.
        let info = VersionInfo {
            current: None,
            wanted: None,
            wanted_major: None,
            latest: Some("2.0.0".to_owned()),
            latest_major: Some("2".to_owned()),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert_eq!(json, r#"{"latest":"2.0.0","latestMajor":"2"}"#);
        assert!(!json.contains("null"));
        assert!(!json.contains("wanted"));
        assert!(!json.contains("current"));
    }

    #[test]
    fn full_version_info_emits_all_five_fields() {
        let info = VersionInfo {
            current: Some("1".to_owned()),
            wanted: Some("1.5.0".to_owned()),
            wanted_major: Some("1".to_owned()),
            latest: Some("2.0.0".to_owned()),
            latest_major: Some("2".to_owned()),
        };
        let value: serde_json::Value = serde_json::to_value(&info).unwrap();
        let obj = value.as_object().unwrap();
        assert_eq!(obj["current"], "1");
        assert_eq!(obj["wanted"], "1.5.0");
        assert_eq!(obj["wantedMajor"], "1");
        assert_eq!(obj["latest"], "2.0.0");
        assert_eq!(obj["latestMajor"], "2");
        assert_eq!(obj.len(), 5);
    }

    #[test]
    fn report_preserves_declaration_order() {
        let stub = || VersionInfo {
            current: Some("1".to_owned()),
            wanted: Some("1.0.0".to_owned()),
            wanted_major: Some("1".to_owned()),
            latest: Some("1.0.0".to_owned()),
            latest_major: Some("1".to_owned()),
        };
        let report = OutdatedReport {
            entries: vec![
                ("ghcr.io/z/last:1".to_owned(), stub()),
                ("ghcr.io/a/first:1".to_owned(), stub()),
            ],
        };
        let json = serde_json::to_string(&OutdatedReportWrapper(&report)).unwrap();
        let z = json.find("z/last").unwrap();
        let a = json.find("a/first").unwrap();
        // Declaration order (z before a) must be preserved, not sorted.
        assert!(z < a, "declaration order must be preserved");
    }

    // -----------------------------------------------------------------------
    // text rendering
    // -----------------------------------------------------------------------

    #[test]
    fn text_table_renders_header_and_dashes_for_missing() {
        let report = OutdatedReport {
            entries: vec![(
                "ghcr.io/devcontainers/features/node:1".to_owned(),
                VersionInfo {
                    current: Some("1".to_owned()),
                    wanted: Some("1.5.0".to_owned()),
                    wanted_major: Some("1".to_owned()),
                    latest: None,
                    latest_major: None,
                },
            )],
        };
        let rows = text_rows(&report);
        let widths = column_widths(&rows);
        let header = format_row(&rows[0], &widths);
        let data = format_row(&rows[1], &widths);
        assert!(header.starts_with("Feature"));
        assert!(header.contains("Current"));
        assert!(data.contains("ghcr.io/devcontainers/features/node"));
        assert!(data.ends_with('-'), "missing latest renders as '-'");
    }

    // -----------------------------------------------------------------------
    // load_version_info — integration (filtering + end-to-end)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn load_version_info_skips_non_oci_local_ref() {
        // A local `./feature` ref must produce no entry; no network is touched
        // because it is filtered out before any registry call.
        let features = vec![("./feature".to_owned(), serde_json::json!({}))];
        let report = load_version_info(&features, None).await;
        assert!(
            report.entries.is_empty(),
            "local refs must not appear in the report"
        );
    }

    #[tokio::test]
    async fn load_version_info_emits_entry_on_unreachable_registry() {
        // A versionable ref whose registry cannot be reached must still appear,
        // with all version fields omitted (no lockfile → no `current` either).
        // `.invalid` is a reserved TLD that never resolves, so this never hits
        // the real network.
        let features = vec![("registry.invalid/x/y:1".to_owned(), serde_json::json!({}))];
        let report = load_version_info(&features, None).await;
        assert_eq!(report.entries.len(), 1, "the feature must still be listed");
        let (key, info) = &report.entries[0];
        assert_eq!(key, "registry.invalid/x/y:1");
        // Empty version list → every field omitted.
        assert_eq!(
            info,
            &VersionInfo {
                current: None,
                wanted: None,
                wanted_major: None,
                latest: None,
                latest_major: None,
            }
        );
        let json = serde_json::to_string(info).unwrap();
        assert_eq!(json, "{}", "an empty entry serializes to an empty object");
    }

    /// End-to-end against the public read-only ghcr registry. Skips when the
    /// network is unavailable. The node feature's tag list is arch-independent,
    /// so no `test_platform()` is needed here.
    #[cella_testing::runtime_test(network)]
    async fn outdated_node_one_from_ghcr() {
        let features = vec![(
            "ghcr.io/devcontainers/features/node:1".to_owned(),
            serde_json::json!({}),
        )];
        let report = load_version_info(&features, None).await;

        let (key, info) = report
            .entries
            .iter()
            .find(|(k, _)| k == "ghcr.io/devcontainers/features/node:1")
            .expect("node:1 must appear in the report");
        assert_eq!(key, "ghcr.io/devcontainers/features/node:1");

        // `latest` is the newest published full-semver tag; `wanted` is the
        // highest 1.x stable; both must be present, as must `latestMajor`.
        let latest = info.latest.as_deref().expect("latest must be present");
        assert!(
            semver::Version::parse(latest).is_ok(),
            "latest '{latest}' must be strict semver"
        );
        let wanted = info.wanted.as_deref().expect("wanted must be present");
        assert!(
            wanted.starts_with("1."),
            "wanted '{wanted}' for tag '1' must be a 1.x version"
        );
        assert!(
            info.latest_major.is_some(),
            "latestMajor must be present alongside latest"
        );
    }

    // -----------------------------------------------------------------------
    // order_features_by_source — config declaration order from raw JSONC
    // -----------------------------------------------------------------------

    #[test]
    fn features_in_source_order_reads_raw_declaration_order() {
        // Keys deliberately NOT alphabetical: node, then git.
        let raw = r#"{
  "features": {
    "ghcr.io/devcontainers/features/node:1": {},
    "ghcr.io/devcontainers/features/git:1": {}
  }
}"#;
        let order = features_in_source_order(raw);
        assert_eq!(
            order,
            vec![
                "ghcr.io/devcontainers/features/node:1".to_owned(),
                "ghcr.io/devcontainers/features/git:1".to_owned(),
            ]
        );
    }

    #[test]
    fn order_features_by_source_restores_declaration_order() {
        // Simulate what `extract_features` hands us: alphabetically sorted
        // (git before node), losing the source order.
        let raw = r#"{
  "features": {
    "ghcr.io/devcontainers/features/node:1": {},
    "ghcr.io/devcontainers/features/git:1": {}
  }
}"#;
        let sorted = vec![
            (
                "ghcr.io/devcontainers/features/git:1".to_owned(),
                serde_json::json!({}),
            ),
            (
                "ghcr.io/devcontainers/features/node:1".to_owned(),
                serde_json::json!({}),
            ),
        ];
        let ordered = order_features_by_source(raw, sorted);
        let keys: Vec<&str> = ordered.iter().map(|(k, _)| k.as_str()).collect();
        // Declaration order (node first) must be restored.
        assert_eq!(
            keys,
            vec![
                "ghcr.io/devcontainers/features/node:1",
                "ghcr.io/devcontainers/features/git:1",
            ]
        );
    }

    #[test]
    fn order_features_by_source_handles_jsonc_comments_and_trailing_commas() {
        let raw = r#"{
  // configured features
  "features": {
    "ghcr.io/devcontainers/features/node:1": {}, // node
    "ghcr.io/devcontainers/features/git:1": {}, // trailing comma ok
  },
}"#;
        let order = features_in_source_order(raw);
        assert_eq!(
            order,
            vec![
                "ghcr.io/devcontainers/features/node:1".to_owned(),
                "ghcr.io/devcontainers/features/git:1".to_owned(),
            ]
        );
    }

    #[test]
    fn order_features_by_source_falls_back_when_unparsable() {
        // Unparseable source → original (input) order is preserved.
        let input = vec![
            ("b:1".to_owned(), serde_json::json!({})),
            ("a:1".to_owned(), serde_json::json!({})),
        ];
        let ordered = order_features_by_source("{ not valid", input);
        let keys: Vec<&str> = ordered.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["b:1", "a:1"]);
    }
}
