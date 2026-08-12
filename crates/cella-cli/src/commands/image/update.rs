//! `cella image update` — check for and apply base image updates.

use clap::Args;
use inquire::Select;

use miette::IntoDiagnostic as _;

use cella_oci::{TagCache, TagSource};

use super::candidates::{self, AxisSet, Candidates, Limitation};
use super::jsonc_edit;
use crate::commands::features::resolve::{self, CommonFeatureFlags};
use crate::commands::{OutputFormat, boxed_err_to_report};
use crate::style;

/// Which axes `--yes` may change without being asked again.
///
/// One struct rather than three loose flags: together they are a single
/// decision — how far an unattended run may move the pin — and they map
/// one-to-one onto [`AxisSet`].
#[derive(Args)]
pub struct AllowFlags {
    /// Allow moving to a newer OS release, not just a newer version.
    #[arg(long)]
    pub allow_os_change: bool,

    /// Allow pinning a floating tag, and making an implicit pin explicit.
    ///
    /// A floating tag re-resolves on every rebuild, so freezing one in CI is
    /// a lasting behaviour change rather than an update.
    #[arg(long)]
    pub allow_pin: bool,

    /// Allow moving to a newer runtime major, e.g. Node 22 to Node 24.
    #[arg(long)]
    pub allow_runtime_change: bool,
}

impl AllowFlags {
    /// The axes these flags permit.
    fn axes(&self) -> AxisSet {
        let mut allowed = AxisSet::default();
        if self.allow_pin {
            allowed |= AxisSet::SHAPE;
        }
        if self.allow_runtime_change {
            allowed |= AxisSet::RUNTIME;
        }
        if self.allow_os_change {
            allowed |= AxisSet::RELEASE;
        }
        allowed
    }
}

/// How much the command may do to the config without being asked again.
///
/// Grouped rather than left loose on [`UpdateArgs`]: these are one
/// decision — report, prompt, or apply — expressed as the flags the official
/// CLI surface expects.
#[derive(Args)]
pub struct ApplyFlags {
    /// Apply the update without prompting. Takes the version bump only
    /// unless the matching --allow-* flag is also given.
    #[arg(long)]
    pub yes: bool,

    #[command(flatten)]
    pub allow: AllowFlags,

    /// Only report; don't apply.
    #[arg(long)]
    pub check: bool,
}

/// Check for and apply devcontainer base image updates.
#[derive(Args)]
pub struct UpdateArgs {
    #[command(flatten)]
    pub common: CommonFeatureFlags,

    #[command(flatten)]
    pub apply: ApplyFlags,

    /// Apply this exact tag instead of choosing interactively.
    ///
    /// Naming a tag is itself the consent, so --allow-os-change is neither
    /// needed nor accepted alongside it, and --check would contradict it.
    #[arg(long, conflicts_with_all = [
        "yes", "check", "allow_os_change", "allow_pin", "allow_runtime_change",
    ])]
    pub to: Option<String>,

    /// Ignore the cached tag list.
    #[arg(long)]
    pub refresh: bool,

    /// Output format (json implies --check unless --yes or --to is given).
    #[arg(long, value_enum, default_value = "text")]
    pub output: OutputFormat,
}

/// How a config's base image is pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageTarget {
    /// Pinned by tag — the only shape this command can update.
    Tagged {
        /// Everything before the tag, e.g. `mcr.microsoft.com/devcontainers/rust`.
        reference: String,
        /// The tag itself, e.g. `2.0.14-trixie`.
        tag: String,
    },
    /// Pinned by digest. Reported and skipped: rewriting a digest is a
    /// different intent, needing a manifest fetch per candidate.
    DigestPinned(String),
}

impl UpdateArgs {
    /// Execute the update command.
    ///
    /// # Errors
    ///
    /// Returns an error on config discovery failure, unreachable registry
    /// with no cached tags, or a failed write.
    ///
    /// Returns [`miette::Report`] rather than a boxed error so the registry
    /// diagnostic's help text survives to the user — boxing erases
    /// [`miette::Diagnostic`].
    pub async fn execute(self) -> miette::Result<()> {
        let config_path = resolve::discover_config(&self.common).map_err(boxed_err_to_report)?;
        let raw = resolve::read_raw_config(&config_path).map_err(boxed_err_to_report)?;
        let stripped = cella_jsonc::strip(&raw).into_diagnostic()?;
        let config: serde_json::Value = serde_json::from_str(&stripped).into_diagnostic()?;

        let target = match classify(&config) {
            Ok(target) => target,
            // Not an error: a Dockerfile- or compose-based config simply has
            // nothing for this command to do.
            Err(message) => {
                eprintln!("{message}");
                return Ok(());
            }
        };

        let (reference, tag) = match target {
            ImageTarget::DigestPinned(image) => {
                eprintln!("{image} is digest-pinned — skipped");
                return Ok(());
            }
            ImageTarget::Tagged { reference, tag } => (reference, tag),
        };

        let cache = TagCache::new();
        // `--to` is validated against this list, and a cache hit can be an
        // hour old — old enough to reject a tag published since. Naming a tag
        // is worth a round trip.
        let refresh = self.refresh || self.to.is_some();
        let fetched = cella_oci::fetch_image_tags(&cache, &reference, refresh).await?;
        if fetched.source == TagSource::StaleCache {
            eprintln!(
                "{} registry unreachable — using cached tags",
                style::warn_mark()
            );
        }

        // An alias line names no runtime, so which one it points at can only
        // be learned from the registry. Probed only when the image actually
        // publishes runtime-ful lines beside it: 1-4 requests, or none.
        let alias_runtime = match candidates::alias_probe(&fetched.tags, &tag) {
            Some(probe) => {
                let resolver = cella_oci::RegistryResolver::new(&reference);
                candidates::resolve_alias_runtime(&resolver, &probe).await
            }
            None => None,
        };

        let computed = candidates::compute(&fetched.tags, &tag, alias_runtime.as_deref());
        if self.stop_at_floating(computed.as_ref()) {
            eprintln!("{tag} tracks latest — nothing to pin");
            return Ok(());
        }
        let found = computed.unwrap_or_else(|| Candidates::unrankable(&tag));

        // `nothing_to_do` implies `reports_only`, so the JSON case falls
        // through to the single render below rather than repeating it here.
        let json_output = matches!(self.output.resolve(), OutputFormat::Json);
        if self.nothing_to_do(&found) && !json_output {
            eprintln!("Base image is up to date.");
            return Ok(());
        }

        if self.reports_only(&found) {
            println!("{}", render_json(&reference, &found)?);
            return Ok(());
        }

        // Skipped when empty: with `--to` we get here having nothing to
        // offer, and an "updates available:" header over no rows is a lie.
        if !json_output && !found.is_empty() {
            display_candidates(&reference, &found);
        }

        if self.apply.check {
            return Ok(());
        }

        let Some(new_tag) = self
            .choose(&found, &fetched.tags)
            .map_err(boxed_err_to_report)?
        else {
            return Ok(());
        };

        let new_image = format!("{reference}:{new_tag}");
        let updated = jsonc_edit::set_image(&raw, &new_image).map_err(boxed_err_to_report)?;
        std::fs::write(&config_path, updated).into_diagnostic()?;

        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "image": {
                        "reference": reference,
                        "previous": tag,
                        "applied": new_tag,
                    }
                }))
                .into_diagnostic()?
            );
        } else {
            eprintln!("{} {tag} -> {new_tag}", style::success_mark());
        }
        Ok(())
    }

    /// Whether JSON output should report and stop.
    ///
    /// `--output json` implies `--check`, but only in the absence of an
    /// explicit instruction to apply: `--to` names a tag and `--yes` accepts
    /// the offered one, and silently discarding either would hand a CI job a
    /// zero exit status and an unchanged file.
    fn reports_only(&self, found: &Candidates) -> bool {
        let applying = self.to.is_some() || (self.apply.yes && !found.is_empty());
        matches!(self.output.resolve(), OutputFormat::Json) && !applying
    }

    /// Whether an unrankable current tag ends the run.
    ///
    /// `latest` names no variant at all, so there is nothing to compute —
    /// but `--to` names a target outright, and applying it is exactly how a
    /// user pins such a tag for the first time. A bare codename does name a
    /// variant and is handled as a pin instead of stopping here.
    const fn stop_at_floating(&self, computed: Option<&Candidates>) -> bool {
        computed.is_none() && self.to.is_none()
    }

    /// Whether to stop with "up to date".
    ///
    /// `--to` names a tag outright, which is consent to apply it whether or
    /// not cella would have offered it — pinning back to a known-good older
    /// tag is a legitimate use and must not be reported as a no-op.
    const fn nothing_to_do(&self, found: &Candidates) -> bool {
        found.is_empty() && self.to.is_none()
    }

    /// Decide which tag to apply, or `None` when the user declined.
    fn choose(
        &self,
        found: &Candidates,
        published: &[String],
    ) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(requested) = &self.to {
            if !published.iter().any(|t| t == requested) {
                return Err(unknown_tag_error(requested, found).into());
            }
            return Ok(Some(requested.clone()));
        }

        if self.apply.yes {
            return Ok(self.auto_choice(found));
        }

        let mut options: Vec<String> = Vec::with_capacity(found.moves.len() + 2);
        if let Some(bump) = &found.version_bump {
            options.push(bump.clone());
        }
        if let Some(pin) = &found.pin {
            options.push(pin.clone());
        }
        for variant_move in &found.moves {
            options.push(format!(
                "{}   {} {} {}  [{}]",
                variant_move.tag,
                variant_move.from,
                style::hint_arrow(),
                variant_move.to,
                variant_move.axes.names().join(", ")
            ));
        }
        let keep = format!("keep {}", found.current);
        options.push(keep.clone());

        let chosen = Select::new("Select a base image tag:", options)
            .with_page_size(15)
            .prompt()?;
        if chosen == keep {
            return Ok(None);
        }
        // The label is `{tag}   {from} → {to}` for OS moves and a bare tag
        // otherwise, so the first whitespace-delimited token is the tag.
        Ok(chosen.split_whitespace().next().map(str::to_owned))
    }

    /// The non-interactive choice under `--yes`.
    ///
    /// A move may be taken only when *every* axis it changes has been
    /// allowed; a partially-allowed move is refused outright rather than
    /// approximated by a smaller one the user did not ask for.
    fn auto_choice(&self, found: &Candidates) -> Option<String> {
        let allowed = self.apply.allow.axes();

        if let Some(best) = found.moves.iter().find(|m| allowed.contains(m.axes)) {
            return Some(best.tag.clone());
        }
        report_blocked(found.moves.iter().map(|m| m.axes), allowed);

        // Pinning a floating tag is itself a shape change: the tag stops
        // tracking upstream. It needs the same consent an explicit move does.
        if let Some(pin) = &found.pin {
            if self.apply.allow.allow_pin {
                return Some(pin.clone());
            }
            eprintln!("(a pin is available; pass --allow-pin)");
            return None;
        }

        found.version_bump.clone()
    }
}

/// Tell the user which flag would unlock the moves that were withheld.
fn report_blocked(axes: impl Iterator<Item = AxisSet>, allowed: AxisSet) {
    let mut blocked = 0;
    let mut needed = AxisSet::default();
    for axis in axes.filter(|a| !allowed.contains(*a)) {
        blocked += 1;
        needed |= axis;
    }
    if blocked == 0 {
        return;
    }

    let flags: Vec<&str> = [
        (AxisSet::SHAPE, "--allow-pin"),
        (AxisSet::RUNTIME, "--allow-runtime-change"),
        (AxisSet::RELEASE, "--allow-os-change"),
    ]
    .into_iter()
    .filter(|(axis, _)| needed.contains(*axis) && !allowed.contains(*axis))
    .map(|(_, flag)| flag)
    .collect();

    eprintln!("({blocked} move(s) available; pass {})", flags.join(" "));
}

/// Read the `"image"` value, or explain which other base-image shape this
/// config uses.
fn current_image(config: &serde_json::Value) -> Result<&str, String> {
    if let Some(image) = config.get("image").and_then(serde_json::Value::as_str) {
        return Ok(image);
    }
    if config.get("dockerComposeFile").is_some() {
        return Err(
            "base image comes from a compose service, not devcontainer.json\n  \
                    help: cella image update only handles the \"image\" property today"
                .to_owned(),
        );
    }
    if config.get("build").is_some() || config.get("dockerFile").is_some() {
        return Err(
            "base image is defined in a Dockerfile, not devcontainer.json\n  \
                    help: cella image update only handles the \"image\" property today"
                .to_owned(),
        );
    }
    Err("devcontainer.json has no \"image\" property — nothing to update".to_owned())
}

/// Classify how the config's base image is pinned.
///
/// The reference is kept exactly as written. Normalization to
/// `docker.io/library/...` happens only when talking to the registry, so the
/// user's own spelling is what gets written back to their file.
fn classify(config: &serde_json::Value) -> Result<ImageTarget, String> {
    let image = current_image(config)?;

    if image.contains('@') {
        return Ok(ImageTarget::DigestPinned(image.to_owned()));
    }

    let (reference, tag) = split_reference(image);
    Ok(ImageTarget::Tagged {
        reference: reference.to_owned(),
        tag: tag.unwrap_or("latest").to_owned(),
    })
}

/// Split an image reference into the part before the tag and the tag.
///
/// The separator is the last `:` that falls after the last `/`, so a registry
/// port survives: `localhost:5000/img:1` splits at the second colon, and
/// `localhost:5000/img` has no tag at all.
fn split_reference(image: &str) -> (&str, Option<&str>) {
    let last_slash = image.rfind('/').map_or(0, |i| i + 1);
    image[last_slash..].rfind(':').map_or((image, None), |off| {
        let colon = last_slash + off;
        (&image[..colon], Some(&image[colon + 1..]))
    })
}

/// Error text for a `--to` tag that the registry does not publish.
fn unknown_tag_error(requested: &str, found: &Candidates) -> String {
    let mut known: Vec<&str> = Vec::new();
    if let Some(bump) = &found.version_bump {
        known.push(bump);
    }
    if let Some(pin) = &found.pin {
        known.push(pin);
    }
    known.extend(found.moves.iter().map(|m| m.tag.as_str()));

    if known.is_empty() {
        return format!("tag not published: {requested}");
    }
    format!(
        "tag not published: {requested}\n  candidates: {}",
        known.join(", ")
    )
}

/// Render the candidate table to stderr.
fn display_candidates(reference: &str, found: &Candidates) {
    eprintln!("{reference}:{} — updates available:", found.current);
    if let Some(bump) = &found.version_bump {
        eprintln!("  {bump}   (version)");
    }
    if let Some(pin) = &found.pin {
        eprintln!("  {pin}   (pin)");
    }
    for variant_move in &found.moves {
        eprintln!(
            "  {}   ({} {} {}: {})",
            variant_move.tag,
            variant_move.from,
            style::hint_arrow(),
            variant_move.to,
            variant_move.axes.names().join(", ")
        );
    }
    // Said out loud rather than silently emitting a shorter list: the user
    // cannot otherwise tell a complete offer from a truncated one.
    if let Some(limitation) = &found.limitation {
        eprintln!("  ({})", limitation.message());
    }
}

/// Render candidates as JSON.
///
/// Deliberately its own shape rather than an addition to `cella outdated`,
/// whose output mirrors the official CLI's `loadVersionInfo` contract.
fn render_json(reference: &str, found: &Candidates) -> miette::Result<String> {
    let moves: Vec<serde_json::Value> = found
        .moves
        .iter()
        .map(|m| {
            serde_json::json!({
                "tag": m.tag,
                "from": m.from,
                "to": m.to,
                "axes": m.axes.names(),
            })
        })
        .collect();

    serde_json::to_string_pretty(&serde_json::json!({
        "image": {
            "reference": reference,
            "current": found.current,
            "versionBump": found.version_bump,
            "pin": found.pin,
            // The CI path is exactly where silently accepting a shortened
            // list costs something, so the degradation travels with it.
            "limitation": found.limitation.as_ref().map(Limitation::message),
            "moves": moves,
        }
    }))
    .into_diagnostic()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::super::release;
    use super::*;

    /// Parse a full CLI invocation down to the `image update` args.
    fn parse_update(argv: &[&str]) -> UpdateArgs {
        use clap::Parser as _;
        match crate::Cli::try_parse_from(argv).unwrap().command {
            crate::commands::Command::Image(args) => match args.command {
                super::super::ImageCommand::Update(update) => update,
            },
            _ => panic!("expected the image subcommand"),
        }
    }

    /// Regression: `?` coerced the registry error into `Box<dyn Error>`, which
    /// erases `Diagnostic`, so the `~/.docker/config.json` help never reached
    /// the user even though the error type carried it.
    #[tokio::test]
    async fn an_unreachable_registry_keeps_its_help_through_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("devcontainer.json");
        std::fs::write(
            &config,
            r#"{"image": "registry.invalid/nobody/nothing:1-trixie"}"#,
        )
        .unwrap();

        let args = parse_update(&[
            "cella",
            "image",
            "update",
            "--check",
            "-f",
            config.to_str().unwrap(),
        ]);
        let err = args
            .execute()
            .await
            .expect_err("an unresolvable registry must fail");

        assert!(
            miette::Diagnostic::help(&*err).is_some(),
            "actionable help must survive the command boundary, got: {err:?}"
        );
    }

    #[test]
    fn reports_the_config_shape_it_cannot_handle() {
        let build = serde_json::json!({"build": {"dockerfile": "Dockerfile"}});
        let err = current_image(&build).unwrap_err();
        assert!(err.contains("Dockerfile"), "got: {err}");

        let compose = serde_json::json!({"dockerComposeFile": "docker-compose.yml"});
        let err = current_image(&compose).unwrap_err();
        assert!(err.contains("compose"), "got: {err}");

        let empty = serde_json::json!({"name": "x"});
        assert!(current_image(&empty).is_err());
    }

    #[test]
    fn digest_pinned_images_are_skipped_not_updated() {
        let cfg = serde_json::json!({"image": "mcr.microsoft.com/devcontainers/rust@sha256:abc"});
        assert!(matches!(classify(&cfg), Ok(ImageTarget::DigestPinned(_))));
    }

    #[test]
    fn keeps_the_reference_exactly_as_written() {
        // Normalization is for talking to the registry, not for editing the
        // user's file: `ubuntu:24.04` must not become `docker.io/library/...`.
        let cfg = serde_json::json!({"image": "ubuntu:24.04"});
        assert_eq!(
            classify(&cfg).unwrap(),
            ImageTarget::Tagged {
                reference: "ubuntu".to_owned(),
                tag: "24.04".to_owned()
            }
        );
    }

    #[test]
    fn a_registry_port_is_not_mistaken_for_a_tag() {
        assert_eq!(
            split_reference("localhost:5000/img:1"),
            ("localhost:5000/img", Some("1"))
        );
        assert_eq!(
            split_reference("localhost:5000/img"),
            ("localhost:5000/img", None)
        );
        assert_eq!(
            split_reference("mcr.microsoft.com/devcontainers/rust:2.0.14-trixie"),
            (
                "mcr.microsoft.com/devcontainers/rust",
                Some("2.0.14-trixie")
            )
        );
    }

    #[test]
    fn an_untagged_image_is_treated_as_floating() {
        let cfg = serde_json::json!({"image": "ubuntu"});
        assert_eq!(
            classify(&cfg).unwrap(),
            ImageTarget::Tagged {
                reference: "ubuntu".to_owned(),
                tag: "latest".to_owned()
            }
        );
        // `latest` has no leading version, so nothing is offered.
        assert!(candidates::compute(&["24.04".to_owned()], "latest", None).is_none());
    }

    /// Regression: `--to` was swallowed by the "up to date" early return, so
    /// pinning back to a known-good older tag silently did nothing.
    #[test]
    fn an_explicit_target_still_applies_when_nothing_is_offered() {
        let up_to_date = Candidates {
            current: "2.0.14-1-trixie".to_owned(),
            version_bump: None,
            pin: None,
            moves: Vec::new(),
            limitation: None,
        };
        assert!(up_to_date.is_empty());

        assert!(
            !parse_update(&["cella", "image", "update", "--to", "2.0.9-trixie"])
                .nothing_to_do(&up_to_date),
            "a named tag is consent to apply it, offered or not"
        );
        assert!(
            parse_update(&["cella", "image", "update"]).nothing_to_do(&up_to_date),
            "without --to, no candidates means nothing to do"
        );
    }

    /// Regression: `compute` returns `None` for a floating tag, and the early
    /// return fired before `--to` was consulted — so pinning `ubuntu:latest`
    /// to a real tag reported "nothing to pin" and changed nothing.
    #[test]
    fn an_explicit_target_survives_a_floating_current_tag() {
        assert!(
            candidates::compute(&["24.04".to_owned()], "latest", None).is_none(),
            "precondition: a floating tag computes nothing"
        );

        assert!(
            !parse_update(&["cella", "image", "update", "--to", "24.04"]).stop_at_floating(None),
            "a named tag is how a floating pin gets fixed"
        );
        assert!(
            parse_update(&["cella", "image", "update"]).stop_at_floating(None),
            "without --to a floating tag really is nothing to do"
        );
    }

    /// Regression: the JSON early return fired before `choose`, so
    /// `--yes --output json` printed candidates, exited 0, and wrote nothing.
    /// A CI job auto-bumping the base image saw success and no change.
    #[test]
    fn json_output_does_not_swallow_an_explicit_apply() {
        let bump = Candidates {
            current: "2.0.2-trixie".to_owned(),
            version_bump: Some("2.0.14-1-trixie".to_owned()),
            pin: None,
            moves: Vec::new(),
            limitation: None,
        };

        assert!(
            !parse_update(&["cella", "image", "update", "--yes", "--output", "json"])
                .reports_only(&bump),
            "--yes is an instruction to apply, whatever the output format"
        );
        assert!(
            parse_update(&["cella", "image", "update", "--output", "json"]).reports_only(&bump),
            "plain --output json still implies --check"
        );
        assert!(
            !parse_update(&[
                "cella",
                "image",
                "update",
                "--to",
                "2.0.14-1-trixie",
                "--output",
                "json"
            ])
            .reports_only(&bump),
            "--to already applied under json and must keep doing so"
        );
    }

    #[test]
    fn unknown_to_tag_lists_the_computed_candidates() {
        let found = Candidates {
            current: "2.0.2-trixie".to_owned(),
            version_bump: Some("2.0.14-1-trixie".to_owned()),
            pin: None,
            moves: Vec::new(),
            limitation: None,
        };
        let err = unknown_tag_error("9.9.9-trixie", &found);
        assert!(err.contains("9.9.9-trixie"));
        assert!(err.contains("2.0.14-1-trixie"), "got: {err}");
    }

    #[test]
    fn json_shape_is_camel_case_and_self_contained() {
        let found = Candidates {
            current: "2.0.2-bookworm".to_owned(),
            version_bump: Some("2.0.14-1-bookworm".to_owned()),
            pin: None,
            moves: vec![candidates::VariantMove {
                tag: "2.0.14-1-trixie".to_owned(),
                from: "bookworm".to_owned(),
                to: "trixie".to_owned(),
                axes: AxisSet::RELEASE,
            }],
            limitation: None,
        };
        let rendered = render_json("mcr.microsoft.com/devcontainers/rust", &found).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["image"]["current"], "2.0.2-bookworm");
        assert_eq!(value["image"]["versionBump"], "2.0.14-1-bookworm");
        assert_eq!(value["image"]["moves"][0]["to"], "trixie");
        assert_eq!(value["image"]["moves"][0]["axes"][0], "release");
        assert_eq!(value["image"]["pin"], serde_json::Value::Null);
        assert_eq!(value["image"]["limitation"], serde_json::Value::Null);
    }

    /// `--yes` may take a move only when every axis it changes is allowed.
    /// A partially-allowed move must be refused, not approximated.
    #[test]
    fn yes_takes_a_move_only_when_every_axis_is_allowed() {
        let found = Candidates {
            current: "5.0.3-22-bookworm".to_owned(),
            version_bump: None,
            pin: None,
            moves: vec![
                candidates::VariantMove {
                    tag: "5.0.3-24-trixie".to_owned(),
                    from: "22-bookworm".to_owned(),
                    to: "24-trixie".to_owned(),
                    axes: AxisSet::RUNTIME | AxisSet::RELEASE,
                },
                candidates::VariantMove {
                    tag: "5.0.3-22-trixie".to_owned(),
                    from: "22-bookworm".to_owned(),
                    to: "22-trixie".to_owned(),
                    axes: AxisSet::RELEASE,
                },
            ],
            limitation: None,
        };

        assert_eq!(
            parse_update(&["cella", "image", "update", "--yes"]).auto_choice(&found),
            None,
            "bare --yes must not change the OS or the runtime"
        );
        assert_eq!(
            parse_update(&["cella", "image", "update", "--yes", "--allow-os-change"])
                .auto_choice(&found)
                .as_deref(),
            Some("5.0.3-22-trixie"),
            "only the release-only move is fully allowed"
        );
        assert_eq!(
            parse_update(&[
                "cella",
                "image",
                "update",
                "--yes",
                "--allow-os-change",
                "--allow-runtime-change",
            ])
            .auto_choice(&found)
            .as_deref(),
            Some("5.0.3-24-trixie"),
            "with both axes allowed the newest reachable move wins"
        );
    }

    /// A shape-only move is digest-identical, so nothing about the resulting
    /// image changes — but it freezes a tag that was tracking upstream, and
    /// that needs consent.
    #[test]
    fn a_shape_only_move_needs_allow_pin() {
        let found = Candidates {
            current: "5.0.3-trixie".to_owned(),
            version_bump: None,
            pin: None,
            moves: vec![candidates::VariantMove {
                tag: "5.0.3-24-trixie".to_owned(),
                from: "trixie".to_owned(),
                to: "24-trixie".to_owned(),
                axes: AxisSet::SHAPE,
            }],
            limitation: None,
        };

        assert_eq!(
            parse_update(&["cella", "image", "update", "--yes"]).auto_choice(&found),
            None,
            "bare --yes must not rewrite tag shape in CI"
        );
        assert_eq!(
            parse_update(&["cella", "image", "update", "--yes", "--allow-pin"])
                .auto_choice(&found)
                .as_deref(),
            Some("5.0.3-24-trixie")
        );
    }

    /// Freezing a floating tag is a lasting behaviour change, so `--yes`
    /// alone leaves it floating.
    #[test]
    fn pinning_a_floating_tag_needs_allow_pin() {
        let found = Candidates {
            current: "24-trixie".to_owned(),
            version_bump: None,
            pin: Some("5.0.3-24-trixie".to_owned()),
            moves: Vec::new(),
            limitation: None,
        };

        assert_eq!(
            parse_update(&["cella", "image", "update", "--yes"]).auto_choice(&found),
            None
        );
        assert_eq!(
            parse_update(&["cella", "image", "update", "--yes", "--allow-pin"])
                .auto_choice(&found)
                .as_deref(),
            Some("5.0.3-24-trixie")
        );
    }

    /// A plain version bump is what `--yes` is for and needs no flag.
    #[test]
    fn yes_alone_still_takes_a_version_bump() {
        let found = Candidates {
            current: "4.0.10-24-trixie".to_owned(),
            version_bump: Some("5.0.3-24-trixie".to_owned()),
            pin: None,
            moves: Vec::new(),
            limitation: None,
        };
        assert_eq!(
            parse_update(&["cella", "image", "update", "--yes"])
                .auto_choice(&found)
                .as_deref(),
            Some("5.0.3-24-trixie")
        );
    }

    /// A `--output json` consumer is the one most likely to accept a short
    /// list without noticing, so the degradation has to reach it too.
    #[test]
    fn json_carries_the_degradation_message() {
        let found = Candidates {
            current: "5.0.3-trixie".to_owned(),
            version_bump: None,
            pin: None,
            moves: Vec::new(),
            limitation: Some(Limitation::UnresolvedAlias("5.0.3-trixie".to_owned())),
        };
        let rendered = render_json("mcr.microsoft.com/devcontainers/typescript-node", &found)
            .expect("json must render");
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        let message = value["image"]["limitation"]
            .as_str()
            .expect("the limitation must survive into json");
        assert!(message.contains("5.0.3-trixie"), "got: {message}");
        assert!(message.contains("could not resolve"), "got: {message}");
    }

    #[test]
    fn cli_accepts_the_documented_flags() {
        use clap::Parser as _;
        assert!(crate::Cli::try_parse_from(["cella", "image", "update", "--check"]).is_ok());
        assert!(
            crate::Cli::try_parse_from(["cella", "image", "update", "--yes", "--allow-os-change"])
                .is_ok()
        );
        assert!(
            crate::Cli::try_parse_from(["cella", "image", "update", "--yes", "--allow-pin"])
                .is_ok()
        );
        assert!(
            crate::Cli::try_parse_from([
                "cella",
                "image",
                "update",
                "--yes",
                "--allow-runtime-change",
                "--allow-os-change",
                "--allow-pin",
            ])
            .is_ok()
        );
        assert!(
            crate::Cli::try_parse_from(["cella", "image", "update", "--to", "2.0.14-trixie"])
                .is_ok()
        );
        assert!(
            crate::Cli::try_parse_from(["cella", "image", "update", "--output", "json"]).is_ok()
        );
    }

    #[test]
    fn to_conflicts_with_the_flags_that_would_contradict_it() {
        use clap::Parser as _;
        for conflicting in [
            vec!["cella", "image", "update", "--to", "x", "--check"],
            vec!["cella", "image", "update", "--to", "x", "--yes"],
            vec!["cella", "image", "update", "--to", "x", "--allow-os-change"],
            vec!["cella", "image", "update", "--to", "x", "--allow-pin"],
            vec![
                "cella",
                "image",
                "update",
                "--to",
                "x",
                "--allow-runtime-change",
            ],
        ] {
            assert!(
                crate::Cli::try_parse_from(&conflicting).is_err(),
                "expected clap to reject {conflicting:?}"
            );
        }
    }

    /// Guards the load-bearing assumption that moves never cross families.
    #[test]
    fn moves_never_cross_distro_families() {
        let debian = release::parse_variant("bookworm").unwrap();
        let ubuntu = release::parse_variant("noble").unwrap();
        assert!(!ubuntu.is_newer_than(&debian));
        assert!(!debian.is_newer_than(&ubuntu));
    }

    /// The alias line resolved against the live registry, end to end: only a
    /// digest can say which runtime `5.0.3-trixie` currently points at.
    #[cella_testing::runtime_test(network)]
    async fn resolves_the_real_alias_line_by_digest() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TagCache::with_root(dir.path());
        let reference = "mcr.microsoft.com/devcontainers/typescript-node";
        let fetched = cella_oci::fetch_image_tags(&cache, reference, true)
            .await
            .unwrap();

        let probe = candidates::alias_probe(&fetched.tags, "5.0.3-trixie")
            .expect("trixie is an alias line upstream");
        let resolver = cella_oci::RegistryResolver::new(reference);
        let runtime = candidates::resolve_alias_runtime(&resolver, &probe)
            .await
            .expect("the alias must resolve to a published runtime");

        let found = candidates::compute(&fetched.tags, "5.0.3-trixie", Some(&runtime)).unwrap();
        assert!(
            found.moves.iter().all(|m| m.axes.contains(AxisSet::SHAPE)),
            "every move off an alias line makes the pin explicit: {:?}",
            found.moves
        );
    }

    #[cella_testing::runtime_test(network)]
    async fn resolves_candidates_for_the_real_rust_image() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TagCache::with_root(dir.path());
        let fetched =
            cella_oci::fetch_image_tags(&cache, "mcr.microsoft.com/devcontainers/rust", true)
                .await
                .unwrap();
        let found = candidates::compute(&fetched.tags, "2.0.2-trixie", None).unwrap();
        let bump = found
            .version_bump
            .expect("a newer trixie tag must exist upstream");
        assert!(bump.ends_with("-trixie"));
    }
}
