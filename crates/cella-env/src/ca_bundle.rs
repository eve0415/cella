//! Host CA bundle detection and container trust store injection.
//!
//! Detects the host's CA trust store and prepares file uploads and commands
//! to inject it into containers, supporting multiple Linux distributions.

use cella_network::ca::{detect_host_ca_bundle, read_additional_ca_cert};

use crate::{FileUpload, PostStartInjection};

/// CA bundle file path inside the container.
const CONTAINER_CA_PATH: &str = "/usr/local/share/ca-certificates/cella-host-ca.crt";

/// Prepare CA bundle injection for a container.
///
/// Returns file uploads and commands to update the container's trust store.
/// Returns `None` if no CA bundle can be detected from the host.
pub fn prepare_ca_injection(additional_ca_path: Option<&str>) -> Option<CaInjection> {
    let mut pem_bundle = String::new();

    // Detect host CA bundle.
    if let Some(host_bundle) = detect_host_ca_bundle() {
        pem_bundle.push_str(&host_bundle.pem_bundle);
    }

    // Append additional CA cert if configured.
    if let Some(ca_path) = additional_ca_path {
        match read_additional_ca_cert(ca_path) {
            Ok(extra_pem) => {
                if !pem_bundle.is_empty() && !pem_bundle.ends_with('\n') {
                    pem_bundle.push('\n');
                }
                pem_bundle.push_str(&extra_pem);
                tracing::info!("Added additional CA cert from {ca_path}");
            }
            Err(e) => {
                tracing::warn!("Failed to read additional CA cert: {e}");
            }
        }
    }

    if pem_bundle.is_empty() {
        tracing::debug!("No CA bundle to inject");
        return None;
    }

    tracing::info!("Preparing CA bundle injection ({} bytes)", pem_bundle.len());

    Some(CaInjection {
        file_upload: FileUpload {
            container_path: CONTAINER_CA_PATH.to_string(),
            content: pem_bundle.into_bytes(),
            mode: 0o644,
        },
    })
}

/// Prepared CA injection data.
pub struct CaInjection {
    /// The CA bundle file to upload.
    pub file_upload: FileUpload,
}

impl CaInjection {
    /// Apply the CA injection to post-start commands.
    ///
    /// Adds the file upload and `update-ca-certificates` command to the
    /// post-start injection. The command is distro-agnostic: tries
    /// `update-ca-certificates` (Debian/Ubuntu/Alpine) first, falls back
    /// to `update-ca-trust` (RHEL/Fedora).
    pub fn apply_to(&self, post_start: &mut PostStartInjection) {
        post_start.file_uploads.push(self.file_upload.clone());

        // Run update-ca-certificates (Debian/Ubuntu/Alpine) or
        // update-ca-trust (RHEL/Fedora). Use a shell command that
        // tries both.
        post_start.git_config_commands.push(vec![
            "sh".to_string(),
            "-c".to_string(),
            "update-ca-certificates 2>/dev/null || update-ca-trust 2>/dev/null || true".to_string(),
        ]);
    }
}
