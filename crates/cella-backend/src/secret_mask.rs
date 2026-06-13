//! Secret-value masking for lifecycle command output.
//!
//! Matches the semantics of the official devcontainer CLI's `maskSecrets` in
//! `src/spec-utils/log.ts`:
//! - Replace each secret **value** (RHS of `KEY=VALUE`) with `********`.
//! - Skip empty values.
//! - Replace longest values first so that a short secret that is a prefix/
//!   substring of a longer one cannot leave partial matches.

/// Eight asterisks — the replacement sentinel used by the official CLI.
const MASK: &str = "********";

/// Replaces secret values in strings passed to lifecycle output sinks.
///
/// Constructed from the raw `KEY=VALUE` secret entries coming out of
/// `--secrets-file`. Calling [`SecretMasker::mask`] on a string is free when
/// there are no secrets (early-return without allocation).
#[derive(Debug, Clone, Default)]
pub struct SecretMasker {
    /// Secret values sorted by descending length (longest first).
    values: Vec<String>,
}

impl SecretMasker {
    /// Build a masker from `KEY=VALUE` secret entries.
    ///
    /// - Values are extracted by splitting on the first `=`.
    /// - Empty values are silently dropped (nothing to mask).
    /// - Values are deduplicated and sorted longest-first.
    pub fn new(entries: &[String]) -> Self {
        // Output is masked per line, so a multiline secret value (e.g. a PEM
        // key in the secrets file) is registered as its individual non-empty
        // lines — otherwise the whole value would never match a single line and
        // leak through.
        let mut values: Vec<String> = entries
            .iter()
            .filter_map(|e| e.split_once('=').map(|(_, v)| v))
            .flat_map(str::lines)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();

        // Dedup before sorting so identical values don't cause double-passes.
        values.sort_unstable();
        values.dedup();

        // Longest first: prevents a shorter secret from being applied first
        // when it is a prefix/substring of a longer one.
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));

        Self { values }
    }

    /// Returns `true` when there are no secrets — callers can skip masking
    /// entirely on the hot path.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Replace every secret value in `s` with `********`.
    ///
    /// Returns the input string unchanged (zero allocation) when no secrets are
    /// registered or no match is found.
    pub fn mask<'s>(&self, s: &'s str) -> std::borrow::Cow<'s, str> {
        if self.values.is_empty() {
            return std::borrow::Cow::Borrowed(s);
        }

        // Delay allocation until the first match so the common no-match path
        // is zero-allocation.
        let mut result: Option<String> = None;
        for secret in &self.values {
            let haystack: &str = result.as_deref().unwrap_or(s);
            if haystack.contains(secret.as_str()) {
                result = Some(haystack.replace(secret.as_str(), MASK));
            }
        }

        result.map_or(std::borrow::Cow::Borrowed(s), |owned| {
            std::borrow::Cow::Owned(owned)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masker(entries: &[&str]) -> SecretMasker {
        SecretMasker::new(&entries.iter().map(ToString::to_string).collect::<Vec<_>>())
    }

    #[test]
    fn single_value_masked() {
        let m = masker(&["TOKEN=secret123"]);
        assert_eq!(
            m.mask("got TOKEN=secret123 here"),
            "got TOKEN=******** here"
        );
    }

    #[test]
    fn multiple_values_all_masked() {
        let m = masker(&["A=alpha", "B=beta"]);
        let out = m.mask("alpha and beta");
        assert_eq!(out, "******** and ********");
    }

    #[test]
    fn longest_first_no_partial_match() {
        // "pass" is a substring of "password". Masking "password" first must
        // not leave "pass" residue from the longer replacement sentinel.
        let m = masker(&["X=pass", "Y=password"]);
        let out = m.mask("my password is pass");
        // "password" → "********", then "pass" → "********"
        assert_eq!(out, "my ******** is ********");
    }

    #[test]
    fn longest_first_prefix_of_longer() {
        // "abc" is prefix of "abcdef". With longest-first, "abcdef" is masked
        // first and "abc" does not partially match inside "********".
        let m = masker(&["S=abc", "T=abcdef"]);
        let out = m.mask("value=abcdef");
        assert_eq!(out, "value=********");
    }

    #[test]
    fn empty_value_skipped() {
        let m = masker(&["EMPTY=", "REAL=hunter2"]);
        let out = m.mask("password is hunter2 and EMPTY= stays");
        assert_eq!(out, "password is ******** and EMPTY= stays");
    }

    #[test]
    fn multiline_value_masked_per_line() {
        // A multiline secret (e.g. a PEM key) registers each non-empty line, so
        // per-line output masking catches it instead of leaking.
        let m = masker(&["KEY=-----BEGIN-----\nMIIByz\n-----END-----"]);
        assert_eq!(m.mask("-----BEGIN-----"), "********");
        assert_eq!(m.mask("MIIByz"), "********");
        assert_eq!(m.mask("-----END-----"), "********");
    }

    #[test]
    fn no_secrets_passthrough() {
        let m = SecretMasker::default();
        let s = "no secrets here";
        // Must return a Borrowed slice — no allocation.
        assert!(matches!(m.mask(s), std::borrow::Cow::Borrowed(_)));
        assert_eq!(m.mask(s), s);
    }

    #[test]
    fn no_match_returns_borrowed() {
        let m = masker(&["KEY=secret"]);
        let s = "nothing to mask";
        assert!(matches!(m.mask(s), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn value_appearing_multiple_times() {
        let m = masker(&["K=tok"]);
        assert_eq!(m.mask("tok and tok again"), "******** and ******** again");
    }

    #[test]
    fn no_key_entry_skipped() {
        // An entry without '=' has no value to extract — skip it gracefully.
        let m = masker(&["NOKEYVALUE"]);
        assert!(m.is_empty());
    }
}
