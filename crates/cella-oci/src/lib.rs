pub mod auth;
pub mod cache;
pub mod extract;

pub use auth::{DockerCredentials, resolve_credentials};
pub use cache::{commit_staging, staging_path};
pub use extract::{extract_layer, is_extractable_layer};

use oci_distribution::secrets::RegistryAuth;
use tracing::debug;

/// Build [`RegistryAuth`] from Docker credential store for the given registry.
pub fn build_registry_auth(registry: &str) -> RegistryAuth {
    let creds = resolve_credentials(registry);
    if let (Some(u), Some(p)) = (creds.username, creds.password) {
        debug!("using basic auth for {registry}");
        RegistryAuth::Basic(u, p)
    } else {
        debug!("no credentials for {registry}; using anonymous auth");
        RegistryAuth::Anonymous
    }
}
