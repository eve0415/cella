//! CA certificate generation and host CA bundle detection.
//!
//! Generates a self-signed CA certificate for MITM TLS interception and
//! detects the host's CA trust store for forwarding into containers.

use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use miette::Diagnostic;
use rcgen::{CertificateParams, DnType, IsCa, KeyPair};
use thiserror::Error;

/// Default storage location for the auto-generated CA.
const CA_DIR: &str = ".cella/proxy";
const CA_CERT_FILE: &str = "ca.pem";
const CA_KEY_FILE: &str = "ca.key";

#[derive(Debug, Error, Diagnostic)]
pub enum CaError {
    #[error("failed to generate CA key pair: {0}")]
    #[diagnostic(code(cella::network::key_generation))]
    KeyGeneration(#[source] rcgen::Error),

    #[error("failed to generate CA certificate: {0}")]
    #[diagnostic(code(cella::network::cert_generation))]
    CertGeneration(#[source] rcgen::Error),

    #[error("failed to write CA files: {0}")]
    #[diagnostic(code(cella::network::io))]
    Io(#[from] std::io::Error),

    #[error("failed to read CA certificate: {0}")]
    #[diagnostic(code(cella::network::read_cert))]
    ReadCert(String),
}

/// A generated or loaded CA certificate and key pair.
#[derive(Debug, Clone)]
pub struct CaCertificate {
    /// PEM-encoded CA certificate.
    pub cert_pem: String,
    /// PEM-encoded CA private key.
    pub key_pem: String,
    /// Path where the CA cert is stored.
    pub cert_path: PathBuf,
    /// Path where the CA key is stored.
    pub key_path: PathBuf,
}

/// Ensure the auto-generated CA exists. Creates it if missing.
///
/// The CA is stored at `~/.cella/proxy/ca.pem` and `~/.cella/proxy/ca.key`.
/// Returns the certificate and key pair.
///
/// # Errors
///
/// Returns `CaError` if CA generation fails or files cannot be read/written.
pub fn ensure_ca() -> Result<CaCertificate, CaError> {
    let ca_dir = ca_directory();
    let cert_path = ca_dir.join(CA_CERT_FILE);
    let key_path = ca_dir.join(CA_KEY_FILE);

    // If both files exist, load and return them.
    if cert_path.exists() && key_path.exists() {
        tracing::debug!("Loading existing CA from {}", ca_dir.display());
        let cert_pem = std::fs::read_to_string(&cert_path)?;
        let key_pem = std::fs::read_to_string(&key_path)?;
        return Ok(CaCertificate {
            cert_pem,
            key_pem,
            cert_path,
            key_path,
        });
    }

    // Generate a new CA.
    tracing::info!("Generating new CA certificate at {}", ca_dir.display());
    generate_ca(&ca_dir)
}

/// Build the `CertificateParams` used for the cella MITM CA.
///
/// Used both when first creating the CA on disk and when re-loading it inside
/// `cella-agent` to sign per-domain leaf certificates. The two call sites
/// MUST produce identical params — otherwise leaf certs end up with an
/// `Issuer` DN that doesn't match the CA's `Subject` DN, and TLS chain
/// validation in the container fails (every MITM'd request gets a
/// connection reset).
#[must_use]
pub fn ca_certificate_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "Cella Dev Container CA");
    params
        .distinguished_name
        .push(DnType::OrganizationName, "Cella");
    params
}

/// Generate a new CA certificate and save to disk.
fn generate_ca(ca_dir: &Path) -> Result<CaCertificate, CaError> {
    std::fs::create_dir_all(ca_dir)?;

    let key_pair = KeyPair::generate().map_err(CaError::KeyGeneration)?;

    let params = ca_certificate_params();

    let cert = params
        .self_signed(&key_pair)
        .map_err(CaError::CertGeneration)?;

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    let cert_path = ca_dir.join(CA_CERT_FILE);
    let key_path = ca_dir.join(CA_KEY_FILE);

    std::fs::write(&cert_path, &cert_pem)?;
    std::fs::write(&key_path, &key_pem)?;

    // Restrict key file permissions on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    }

    tracing::info!("CA certificate generated: {}", cert_path.display());

    Ok(CaCertificate {
        cert_pem,
        key_pem,
        cert_path,
        key_path,
    })
}

/// Get the CA storage directory (`~/.cella/proxy/`).
fn ca_directory() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    Path::new(&home).join(CA_DIR)
}

/// Detected host CA bundle for injection into containers.
#[derive(Debug, Clone)]
pub struct HostCaBundle {
    /// PEM-encoded certificate bundle (concatenated PEM blocks).
    pub pem_bundle: String,
}

/// Detect and read the host's CA trust store.
///
/// Tries multiple platform-specific locations. Returns `None` if
/// no CA bundle can be found.
pub fn detect_host_ca_bundle() -> Option<HostCaBundle> {
    // Try platform-native detection first (handles macOS Keychain, etc.)
    if let Some(bundle) = detect_via_native_certs() {
        return Some(bundle);
    }

    // Fallback to well-known file paths.
    detect_from_known_paths()
}

/// Use `rustls-native-certs` to read the platform's trust store.
fn detect_via_native_certs() -> Option<HostCaBundle> {
    let certs = rustls_native_certs::load_native_certs();

    if !certs.errors.is_empty() {
        for err in &certs.errors {
            tracing::warn!("Error loading native cert: {err}");
        }
    }

    if certs.certs.is_empty() {
        return None;
    }

    let mut pem_bundle = String::new();
    for cert in &certs.certs {
        // Convert DER to PEM.
        let b64 = BASE64.encode(cert.as_ref());
        pem_bundle.push_str("-----BEGIN CERTIFICATE-----\n");
        // Wrap at 76 characters per PEM spec.
        for chunk in b64.as_bytes().chunks(76) {
            pem_bundle.push_str(std::str::from_utf8(chunk).unwrap_or(""));
            pem_bundle.push('\n');
        }
        pem_bundle.push_str("-----END CERTIFICATE-----\n");
    }

    tracing::info!("Detected {} host CA certificates", certs.certs.len());
    Some(HostCaBundle { pem_bundle })
}

/// Try to read CA bundle from well-known filesystem paths.
fn detect_from_known_paths() -> Option<HostCaBundle> {
    let known_paths = [
        "/etc/ssl/certs/ca-certificates.crt", // Debian/Ubuntu
        "/etc/pki/tls/certs/ca-bundle.crt",   // RHEL/CentOS
        "/etc/ssl/ca-bundle.pem",             // openSUSE
        "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem", // Fedora
        "/etc/ssl/cert.pem",                  // macOS / Alpine
    ];

    for path_str in &known_paths {
        let path = Path::new(path_str);
        if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(content) if !content.is_empty() => {
                    tracing::info!("Read host CA bundle from {}", path.display());
                    return Some(HostCaBundle {
                        pem_bundle: content,
                    });
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!("Could not read {}: {e}", path.display());
                }
            }
        }
    }

    None
}

/// Read an additional CA certificate file specified by the user.
///
/// # Errors
///
/// Returns `CaError::ReadCert` if the file cannot be read.
pub fn read_additional_ca_cert(path: &str) -> Result<String, CaError> {
    let path = Path::new(path);
    std::fs::read_to_string(path).map_err(|e| {
        CaError::ReadCert(format!(
            "failed to read CA cert from {}: {e}",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_directory_uses_home() {
        let dir = ca_directory();
        assert!(dir.to_string_lossy().contains(".cella/proxy"));
    }

    #[test]
    fn detect_from_known_paths_doesnt_panic() {
        let _ = detect_from_known_paths();
    }

    #[test]
    fn generate_and_load_ca() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ca = generate_ca(tmp.path()).unwrap();

        assert!(ca.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(ca.key_pem.contains("BEGIN"));
        assert!(ca.cert_path.exists());
        assert!(ca.key_path.exists());

        // Loading should return the same cert.
        let cert_pem = std::fs::read_to_string(&ca.cert_path).unwrap();
        assert_eq!(cert_pem, ca.cert_pem);
    }
}
