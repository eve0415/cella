pub mod auth;
pub mod cache;
pub mod extract;
pub mod inspect;
pub mod limits;
pub mod push;

pub use auth::{DockerCredentials, resolve_credentials};
pub use cache::{commit_staging, staging_path};
pub use extract::{ExtractionError, extract_layer, is_extractable_layer};
pub use inspect::{fetch_manifest_with_digest, fetch_published_tags, parse_reference};
pub use limits::{
    LimitedReader, LimitedWriter, MAX_BLOB_COMPRESSED_BYTES, MAX_BLOB_DECOMPRESSED_BYTES,
    MAX_COLLECTION_JSON_BYTES, would_exceed_cap,
};
pub use push::{LayerSpec, PushError, PushResult, list_published_tags, push_artifact};

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
