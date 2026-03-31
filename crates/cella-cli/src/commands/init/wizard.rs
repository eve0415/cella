//! Interactive wizard for `cella init`.
//!
//! Guides the user through template selection, option configuration,
//! feature selection, and config generation using inquire prompts.

use crate::progress::Progress;

use super::InitArgs;

/// Run the interactive init wizard.
///
/// # Errors
///
/// Returns errors for network failures, user cancellation, or I/O errors.
#[expect(
    clippy::unused_async,
    reason = "will be async when wizard is implemented"
)]
pub async fn run(_args: InitArgs, _progress: Progress) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("cella init: interactive wizard not yet implemented");
    eprintln!("Use --template <ref> for non-interactive mode.");
    Err("interactive mode not yet implemented".into())
}
