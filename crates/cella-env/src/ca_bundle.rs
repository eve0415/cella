//! Host CA bundle detection and container trust store injection.
//!
//! Detects the host's CA trust store and prepares file uploads and commands
//! to inject it into containers, supporting multiple Linux distributions.

use cella_network::ca::{detect_host_ca_bundle, read_additional_ca_cert};

use crate::{FileUpload, PostStartInjection};

/// Container OS family, detected via `/etc/os-release`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerDistro {
    Debian,
    Rhel,
    Unknown,
}

impl ContainerDistro {
    /// Detect the container OS family from the output of `cat /etc/os-release`.
    pub fn from_os_release(content: &str) -> Self {
        let lower = content.to_ascii_lowercase();
        // Check ID= and ID_LIKE= lines for known families.
        for line in lower.lines() {
            if let Some(id) = line
                .strip_prefix("id=")
                .or_else(|| line.strip_prefix("id_like="))
            {
                let id = id.trim_matches('"');
                if id.contains("debian")
                    || id.contains("ubuntu")
                    || id.contains("alpine")
                    || id.contains("mint")
                {
                    return Self::Debian;
                }
                if id.contains("rhel")
                    || id.contains("fedora")
                    || id.contains("centos")
                    || id.contains("rocky")
                    || id.contains("alma")
                    || id.contains("oracle")
                    || id.contains("amzn")
                    || id.contains("suse")
                {
                    return Self::Rhel;
                }
            }
        }
        Self::Unknown
    }

    /// The CA certificate path for this distro family.
    pub fn ca_cert_path(&self, filename: &str) -> String {
        match self {
            Self::Debian | Self::Unknown => {
                format!("/usr/local/share/ca-certificates/{filename}")
            }
            Self::Rhel => format!("/etc/pki/ca-trust/source/anchors/{filename}"),
        }
    }

    /// The trust store update command for this distro family.
    /// Returns a shell command that runs the appropriate tool.
    pub fn trust_store_update_command(&self) -> Vec<String> {
        match self {
            Self::Debian => vec![
                "sh".to_string(),
                "-c".to_string(),
                "update-ca-certificates 2>/dev/null || true".to_string(),
            ],
            Self::Rhel => vec![
                "sh".to_string(),
                "-c".to_string(),
                "update-ca-trust 2>/dev/null || true".to_string(),
            ],
            Self::Unknown => vec![
                "sh".to_string(),
                "-c".to_string(),
                "update-ca-certificates 2>/dev/null || update-ca-trust 2>/dev/null || true"
                    .to_string(),
            ],
        }
    }
}

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
        pem_content: pem_bundle.into_bytes(),
    })
}

/// Prepared CA injection data.
pub struct CaInjection {
    /// The CA bundle PEM content to upload.
    pub pem_content: Vec<u8>,
}

impl CaInjection {
    /// Apply the CA injection to post-start commands.
    ///
    /// Uploads the CA bundle to the distro-appropriate path and adds
    /// the trust store update command to `root_commands` (requires root).
    pub fn apply_to(&self, post_start: &mut PostStartInjection, distro: &ContainerDistro) {
        let ca_path = distro.ca_cert_path("cella-host-ca.crt");
        post_start.file_uploads.push(FileUpload {
            container_path: ca_path,
            content: self.pem_content.clone(),
            mode: 0o644,
        });

        // For unknown distro, also upload to the other path.
        if *distro == ContainerDistro::Unknown {
            post_start.file_uploads.push(FileUpload {
                container_path: "/etc/pki/ca-trust/source/anchors/cella-host-ca.crt".to_string(),
                content: self.pem_content.clone(),
                mode: 0o644,
            });
        }

        post_start
            .root_commands
            .push(distro.trust_store_update_command());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_debian() {
        let os_release = r#"NAME="Ubuntu"
ID=ubuntu
ID_LIKE=debian
VERSION_ID="22.04"
"#;
        assert_eq!(
            ContainerDistro::from_os_release(os_release),
            ContainerDistro::Debian
        );
    }

    #[test]
    fn detect_rhel() {
        let os_release = r#"NAME="Rocky Linux"
ID="rocky"
ID_LIKE="rhel centos fedora"
"#;
        assert_eq!(
            ContainerDistro::from_os_release(os_release),
            ContainerDistro::Rhel
        );
    }

    #[test]
    fn detect_alpine() {
        let os_release = r#"NAME="Alpine Linux"
ID=alpine
"#;
        assert_eq!(
            ContainerDistro::from_os_release(os_release),
            ContainerDistro::Debian
        );
    }

    #[test]
    fn detect_unknown() {
        let os_release = r#"NAME="Custom OS"
ID=custom
"#;
        assert_eq!(
            ContainerDistro::from_os_release(os_release),
            ContainerDistro::Unknown
        );
    }

    #[test]
    fn ca_cert_paths() {
        assert_eq!(
            ContainerDistro::Debian.ca_cert_path("test.crt"),
            "/usr/local/share/ca-certificates/test.crt"
        );
        assert_eq!(
            ContainerDistro::Rhel.ca_cert_path("test.crt"),
            "/etc/pki/ca-trust/source/anchors/test.crt"
        );
    }

    #[test]
    fn trust_store_commands() {
        let cmd = ContainerDistro::Debian.trust_store_update_command();
        assert!(cmd[2].contains("update-ca-certificates"));

        let cmd = ContainerDistro::Rhel.trust_store_update_command();
        assert!(cmd[2].contains("update-ca-trust"));

        let cmd = ContainerDistro::Unknown.trust_store_update_command();
        assert!(cmd[2].contains("update-ca-certificates"));
        assert!(cmd[2].contains("update-ca-trust"));
    }
}
