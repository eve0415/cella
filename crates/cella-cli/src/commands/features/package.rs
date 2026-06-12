//! `cella features package` — pack local feature sources into distributable tarballs.

use std::path::PathBuf;

use clap::Args;

use cella_features::package::{self, PackageOptions};

/// Package one or more devcontainer features into distributable tarballs.
#[derive(Args)]
pub struct PackageArgs {
    /// Path to a single feature directory or a collection root (default: `.`).
    #[arg(default_value = ".")]
    pub target: PathBuf,

    /// Directory where tarballs and `devcontainer-collection.json` are written.
    #[arg(short = 'o', long, default_value = "./output")]
    pub output_folder: PathBuf,

    /// Delete the output folder before packaging if it already exists.
    #[arg(short = 'f', long)]
    pub force_clean_output_folder: bool,

    /// Log verbosity level.
    #[arg(long, value_enum, default_value_t = crate::commands::LogLevel::Info)]
    pub log_level: crate::commands::LogLevel,
}

impl PackageArgs {
    /// Execute the package command.
    ///
    /// # Errors
    ///
    /// Returns an error if packaging fails (missing files, validation errors, I/O).
    pub fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let opts = PackageOptions {
            target: self.target,
            output_folder: self.output_folder,
            force_clean_output_folder: self.force_clean_output_folder,
        };

        match package::package(&opts) {
            Ok(result) => {
                let n = result.features.len();
                if n == 1 {
                    eprintln!("Packaged feature '{}'", result.features[0].id);
                } else {
                    eprintln!("Packaged {n} features!");
                }
                Ok(())
            }
            Err(e) => {
                eprintln!("{e}");
                eprintln!("Failed to package features");
                Err(Box::new(e))
            }
        }
    }
}
