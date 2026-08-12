//! Manifest-digest resolution for image-tag aliases.
//!
//! Some registries publish a floating variant and an explicit runtime line
//! under the same manifest. Comparing their digests lets callers identify
//! that relationship without guessing from tag names.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Mutex, PoisonError};

use tracing::debug;

/// Resolves a tag to its manifest digest, so callers can tell which concrete
/// line an alias tag points at.
pub trait AliasResolver {
    /// The manifest digest for `tag`, or `None` when it cannot be resolved.
    fn digest(&self, tag: &str) -> impl Future<Output = Option<String>> + Send;
}

/// Resolves alias tags against an OCI registry.
///
/// Results are cached for the resolver's lifetime, including failed probes,
/// because one command may compare the same tag against several candidates.
#[derive(Debug)]
pub struct RegistryResolver {
    image: String,
    cache: Mutex<HashMap<String, Option<String>>>,
}

impl RegistryResolver {
    /// Create a resolver for all tags under `image`.
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    async fn resolve_with<F, Fut>(&self, tag: &str, fetch: F) -> Option<String>
    where
        F: FnOnce(String) -> Fut + Send,
        Fut: Future<Output = Option<String>> + Send,
    {
        let cached = {
            let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            cache.get(tag).cloned()
        };
        if let Some(digest) = cached {
            debug!("alias digest cache hit for {}:{tag}", self.image);
            return digest;
        }

        let reference = format!("{}:{tag}", self.image);
        debug!("fetching alias digest for {reference}");
        let digest = fetch(reference).await;

        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache.entry(tag.to_owned()).or_insert(digest).clone()
    }
}

impl AliasResolver for RegistryResolver {
    fn digest(&self, tag: &str) -> impl Future<Output = Option<String>> + Send {
        self.resolve_with(tag, |reference| async move {
            match crate::fetch_manifest_with_digest(&reference).await {
                Ok((_, digest)) => Some(digest),
                Err(error) => {
                    debug!("failed to resolve alias digest for {reference}: {error}");
                    None
                }
            }
        })
    }
}

/// In-memory resolver for deterministic alias-resolution tests.
///
/// Only seeded tags resolve, which lets callers describe the exact registry
/// state relevant to a test without network access.
#[derive(Debug, Clone)]
pub struct MapResolver {
    digests: HashMap<String, String>,
}

impl MapResolver {
    /// Create a resolver from `(tag, digest)` pairs.
    pub fn new<I, T, D>(entries: I) -> Self
    where
        I: IntoIterator<Item = (T, D)>,
        T: Into<String>,
        D: Into<String>,
    {
        Self {
            digests: entries
                .into_iter()
                .map(|(tag, digest)| (tag.into(), digest.into()))
                .collect(),
        }
    }
}

impl AliasResolver for MapResolver {
    fn digest(&self, tag: &str) -> impl Future<Output = Option<String>> + Send {
        std::future::ready(self.digests.get(tag).cloned())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn map_resolver_returns_only_seeded_digests() {
        let resolver = MapResolver::new([
            ("5.0.3-trixie", "alias-digest"),
            ("5.0.3-24-trixie", "explicit-digest"),
        ]);

        assert_eq!(
            resolver.digest("5.0.3-trixie").await.as_deref(),
            Some("alias-digest")
        );
        assert_eq!(resolver.digest("unseeded").await, None);
    }

    #[tokio::test]
    async fn registry_errors_degrade_to_none() {
        let resolver = RegistryResolver::new("registry.invalid/nobody/nothing");

        assert_eq!(resolver.digest("latest").await, None);
    }

    #[tokio::test]
    async fn registry_resolver_caches_the_first_probe() {
        let resolver = RegistryResolver::new("registry.example/image");
        let fetches = AtomicUsize::new(0);

        let first = resolver
            .resolve_with("tag", |_| async {
                fetches.fetch_add(1, Ordering::Relaxed);
                Some("digest".to_owned())
            })
            .await;
        let second = resolver
            .resolve_with("tag", |_| async {
                fetches.fetch_add(1, Ordering::Relaxed);
                Some("different".to_owned())
            })
            .await;

        assert_eq!(first.as_deref(), Some("digest"));
        assert_eq!(second, first);
        assert_eq!(fetches.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn registry_resolver_caches_failed_probes() {
        let resolver = RegistryResolver::new("registry.example/image");
        let fetches = AtomicUsize::new(0);

        let first = resolver
            .resolve_with("missing", |_| async {
                fetches.fetch_add(1, Ordering::Relaxed);
                None
            })
            .await;
        let second = resolver
            .resolve_with("missing", |_| async {
                fetches.fetch_add(1, Ordering::Relaxed);
                Some("unexpected".to_owned())
            })
            .await;

        assert_eq!(first, None);
        assert_eq!(second, None);
        assert_eq!(fetches.load(Ordering::Relaxed), 1);
    }

    #[cella_testing::runtime_test(network)]
    async fn alias_and_explicit_lines_share_a_digest() {
        let resolver = RegistryResolver::new("mcr.microsoft.com/devcontainers/typescript-node");

        let alias = resolver.digest("5.0.3-trixie").await.unwrap();
        let explicit = resolver.digest("5.0.3-24-trixie").await.unwrap();

        assert!(!alias.is_empty());
        assert_eq!(alias, explicit);
    }
}
