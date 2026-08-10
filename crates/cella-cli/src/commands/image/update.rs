//! `cella image update` — check for and apply base image updates.

use clap::Args;
use inquire::Select;

use miette::IntoDiagnostic as _;

use cella_oci::{TagCache, TagSource};

use super::candidates::{self, Candidates};
use super::jsonc_edit;
use crate::commands::features::resolve::{self, CommonFeatureFlags};
use crate::commands::{OutputFormat, boxed_err_to_report};

/// How much the command may do to the config without being asked again.
///
/// Grouped rather than left loose on [`UpdateArgs`]: these three are one
/// decision — report, prompt, or apply — expressed as the flags the official
/// CLI surface expects.
#[derive(Args)]
pub struct ApplyFlags {
    /// Apply the update without prompting. Takes the version bump only
    /// unless --allow-os-change is also given.
    #[arg(long)]
    pub yes: bool,

    /// Allow moving to a newer OS release, not just a newer version.
    #[arg(long)]
    pub allow_os_change: bool,

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
    #[arg(long, conflicts_with_all = ["yes", "check", "allow_os_change"])]
    pub to: Option<String>,

    /// Ignore the cached tag list.
    #[arg(long)]
    pub refresh: bool,

    /// Output format (json implies --check).
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
        let fetched = cella_oci::fetch_image_tags(&cache, &reference, self.refresh).await?;
        if fetched.source == TagSource::StaleCache {
            eprintln!("\u{26a0} registry unreachable — using cached tags");
        }

        let Some(found) = candidates::compute(&fetched.tags, &tag) else {
            eprintln!("{tag} tracks latest — nothing to pin");
            return Ok(());
        };

        if self.nothing_to_do(&found) {
            if matches!(self.output.resolve(), OutputFormat::Json) {
                println!("{}", render_json(&reference, &found)?);
            } else {
                eprintln!("Base image is up to date.");
            }
            return Ok(());
        }

        // `--to` names a tag outright, so it applies even under --output json;
        // clap already rejects `--to` with `--check`.
        let json_output = matches!(self.output.resolve(), OutputFormat::Json);
        if json_output && self.to.is_none() {
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
            eprintln!("\u{2713} {tag} -> {new_tag}");
        }
        Ok(())
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

        let mut options: Vec<String> = Vec::with_capacity(found.os_moves.len() + 2);
        if let Some(bump) = &found.version_bump {
            options.push(bump.clone());
        }
        for os_move in &found.os_moves {
            options.push(format!(
                "{}   {} \u{2192} {}",
                os_move.tag, os_move.from, os_move.to
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
    fn auto_choice(&self, found: &Candidates) -> Option<String> {
        if self.apply.allow_os_change
            && let Some(best) = found.os_moves.first()
        {
            return Some(best.tag.clone());
        }
        if found.version_bump.is_none() && !found.os_moves.is_empty() {
            eprintln!(
                "({} OS move(s) available; pass --allow-os-change)",
                found.os_moves.len()
            );
            return None;
        }
        if !found.os_moves.is_empty() {
            eprintln!(
                "({} OS move(s) available; pass --allow-os-change)",
                found.os_moves.len()
            );
        }
        found.version_bump.clone()
    }
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
    known.extend(found.os_moves.iter().map(|m| m.tag.as_str()));

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
    for os_move in &found.os_moves {
        eprintln!(
            "  {}   ({} \u{2192} {})",
            os_move.tag, os_move.from, os_move.to
        );
    }
}

/// Render candidates as JSON.
///
/// Deliberately its own shape rather than an addition to `cella outdated`,
/// whose output mirrors the official CLI's `loadVersionInfo` contract.
fn render_json(reference: &str, found: &Candidates) -> miette::Result<String> {
    let os_moves: Vec<serde_json::Value> = found
        .os_moves
        .iter()
        .map(|m| serde_json::json!({"tag": m.tag, "from": m.from, "to": m.to}))
        .collect();

    serde_json::to_string_pretty(&serde_json::json!({
        "image": {
            "reference": reference,
            "current": found.current,
            "versionBump": found.version_bump,
            "osMoves": os_moves,
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
        assert!(candidates::compute(&["24.04".to_owned()], "latest").is_none());
    }

    /// Regression: `--to` was swallowed by the "up to date" early return, so
    /// pinning back to a known-good older tag silently did nothing.
    #[test]
    fn an_explicit_target_still_applies_when_nothing_is_offered() {
        let up_to_date = Candidates {
            current: "2.0.14-1-trixie".to_owned(),
            version_bump: None,
            os_moves: Vec::new(),
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

    #[test]
    fn unknown_to_tag_lists_the_computed_candidates() {
        let found = Candidates {
            current: "2.0.2-trixie".to_owned(),
            version_bump: Some("2.0.14-1-trixie".to_owned()),
            os_moves: Vec::new(),
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
            os_moves: vec![candidates::OsMove {
                tag: "2.0.14-1-trixie".to_owned(),
                from: "bookworm".to_owned(),
                to: "trixie".to_owned(),
            }],
        };
        let rendered = render_json("mcr.microsoft.com/devcontainers/rust", &found).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["image"]["current"], "2.0.2-bookworm");
        assert_eq!(value["image"]["versionBump"], "2.0.14-1-bookworm");
        assert_eq!(value["image"]["osMoves"][0]["to"], "trixie");
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
        ] {
            assert!(
                crate::Cli::try_parse_from(&conflicting).is_err(),
                "expected clap to reject {conflicting:?}"
            );
        }
    }

    /// Guards the load-bearing assumption that OS moves never cross families.
    #[test]
    fn os_moves_never_cross_distro_families() {
        let debian = release::parse_variant("bookworm").unwrap();
        let ubuntu = release::parse_variant("noble").unwrap();
        assert!(!ubuntu.is_newer_than(&debian));
        assert!(!debian.is_newer_than(&ubuntu));
    }

    #[cella_testing::runtime_test(network)]
    async fn resolves_candidates_for_the_real_rust_image() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TagCache::with_root(dir.path());
        let fetched =
            cella_oci::fetch_image_tags(&cache, "mcr.microsoft.com/devcontainers/rust", true)
                .await
                .unwrap();
        let found = candidates::compute(&fetched.tags, "2.0.2-trixie").unwrap();
        let bump = found
            .version_bump
            .expect("a newer trixie tag must exist upstream");
        assert!(bump.ends_with("-trixie"));
    }
}
