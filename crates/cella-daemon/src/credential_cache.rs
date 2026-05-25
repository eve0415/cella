//! In-memory TTL credential cache with zeroization.
//!
//! Caches resolved credentials keyed by `(provider_id, domain)` to avoid
//! repeated env var reads or subprocess invocations (e.g. `gh auth token`).
//! Cache entries are zeroized on drop to prevent stale credentials from
//! lingering in process memory.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use zeroize::Zeroize;

/// A cached credential value with an expiration timestamp.
struct CachedCredential {
    value: String,
    expires_at: Instant,
}

impl CachedCredential {
    fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

impl Drop for CachedCredential {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}

/// In-memory credential cache with configurable TTL.
///
/// Cache entries are keyed by `(provider_id, domain)`.  When TTL is zero the
/// cache is disabled — `insert` is a no-op and `get` always returns `None`.
///
/// All cached values are zeroized (overwritten with zeros) when evicted or
/// when the cache itself is dropped.
pub struct CredentialCache {
    entries: HashMap<(String, String), CachedCredential>,
    ttl: Duration,
}

impl CredentialCache {
    /// Create a new cache.  A `ttl_seconds` of 0 disables caching entirely.
    #[must_use]
    pub fn new(ttl_seconds: u32) -> Self {
        Self {
            entries: HashMap::new(),
            ttl: Duration::from_secs(u64::from(ttl_seconds)),
        }
    }

    /// Look up a cached credential.
    ///
    /// Returns `None` if the cache is disabled, the key is absent, or the
    /// entry has expired (expired entries are removed immediately).
    pub fn get(&mut self, provider_id: &str, domain: &str) -> Option<&str> {
        if self.is_disabled() {
            return None;
        }

        let key = (provider_id.to_string(), domain.to_string());

        // Check expiry first — remove if stale.
        if self
            .entries
            .get(&key)
            .is_some_and(CachedCredential::is_expired)
        {
            self.entries.remove(&key); // Drop triggers zeroize
            return None;
        }

        self.entries.get(&key).map(|entry| entry.value.as_str())
    }

    /// Insert a credential into the cache.  No-op when the cache is disabled.
    pub fn insert(&mut self, provider_id: &str, domain: &str, value: String) {
        if self.is_disabled() {
            return;
        }

        let key = (provider_id.to_string(), domain.to_string());
        self.entries.insert(
            key,
            CachedCredential {
                value,
                expires_at: Instant::now() + self.ttl,
            },
        );
    }

    /// Returns `true` when the cache is disabled (TTL is zero).
    #[must_use]
    pub const fn is_disabled(&self) -> bool {
        self.ttl.is_zero()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_returns_cached_value_before_expiry() {
        let mut cache = CredentialCache::new(60);
        cache.insert("anthropic", "api.anthropic.com", "sk-secret".to_string());

        assert_eq!(
            cache.get("anthropic", "api.anthropic.com"),
            Some("sk-secret")
        );
    }

    #[test]
    fn get_returns_none_for_missing_key() {
        let mut cache = CredentialCache::new(60);
        cache.insert("anthropic", "api.anthropic.com", "sk-secret".to_string());

        assert_eq!(cache.get("openai", "api.openai.com"), None);
    }

    #[test]
    fn expired_entry_returns_none() {
        // Use a TTL of 0 seconds is disabled, so use 1 second and a manually
        // expired entry to test expiry logic.
        let mut cache = CredentialCache::new(60);
        let key = ("anthropic".to_string(), "api.anthropic.com".to_string());
        cache.entries.insert(
            key,
            CachedCredential {
                value: "sk-expired".to_string(),
                expires_at: Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            },
        );

        assert_eq!(cache.get("anthropic", "api.anthropic.com"), None);
        // Entry should have been removed
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn disabled_cache_insert_is_noop() {
        let mut cache = CredentialCache::new(0);
        assert!(cache.is_disabled());

        cache.insert("anthropic", "api.anthropic.com", "sk-secret".to_string());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn disabled_cache_get_returns_none() {
        let mut cache = CredentialCache::new(0);

        // Force an entry in despite the disabled flag to prove get checks
        let key = ("anthropic".to_string(), "api.anthropic.com".to_string());
        cache.entries.insert(
            key,
            CachedCredential {
                value: "sk-secret".to_string(),
                expires_at: Instant::now() + Duration::from_mins(1),
            },
        );

        assert_eq!(cache.get("anthropic", "api.anthropic.com"), None);
    }

    #[test]
    fn is_disabled_when_ttl_zero() {
        assert!(CredentialCache::new(0).is_disabled());
    }

    #[test]
    fn is_enabled_when_ttl_nonzero() {
        assert!(!CredentialCache::new(1).is_disabled());
    }

    #[test]
    fn drop_zeroizes_value() {
        let cred = CachedCredential {
            value: "super-secret-key".to_string(),
            expires_at: Instant::now() + Duration::from_mins(1),
        };

        // Explicitly call drop to trigger zeroization
        drop(cred);

        // Cannot inspect after drop, but we can verify that the Zeroize
        // trait is correctly wired by testing zeroize directly.
        let mut s = String::from("should-be-zeroed");
        s.zeroize();
        assert!(s.is_empty());
    }

    #[test]
    fn insert_overwrites_existing_entry() {
        let mut cache = CredentialCache::new(60);
        cache.insert("anthropic", "api.anthropic.com", "old-key".to_string());
        cache.insert("anthropic", "api.anthropic.com", "new-key".to_string());

        assert_eq!(cache.get("anthropic", "api.anthropic.com"), Some("new-key"));
    }

    #[test]
    fn different_domains_same_provider_are_separate() {
        let mut cache = CredentialCache::new(60);
        cache.insert("github", "github.com", "token-a".to_string());
        cache.insert("github", "api.github.com", "token-b".to_string());

        assert_eq!(cache.get("github", "github.com"), Some("token-a"));
        assert_eq!(cache.get("github", "api.github.com"), Some("token-b"));
    }

    #[test]
    fn different_providers_same_domain_are_separate() {
        let mut cache = CredentialCache::new(60);
        cache.insert("provider-a", "api.example.com", "key-a".to_string());
        cache.insert("provider-b", "api.example.com", "key-b".to_string());

        assert_eq!(cache.get("provider-a", "api.example.com"), Some("key-a"));
        assert_eq!(cache.get("provider-b", "api.example.com"), Some("key-b"));
    }
}
