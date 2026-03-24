//! Host credential invocation — thin wrapper around `cella_daemon::credential`.
//!
//! Maps `CellaDaemonError` to `CellaCredentialProxyError`.

use std::collections::HashMap;

use crate::CellaCredentialProxyError;

/// Invoke the host's git credential helper, mapping daemon errors to proxy errors.
///
/// # Errors
///
/// Returns `CellaCredentialProxyError::GitCredential` if the git credential command fails.
pub fn invoke_git_credential<S: std::hash::BuildHasher>(
    operation: &str,
    fields: &HashMap<String, String, S>,
) -> Result<HashMap<String, String>, CellaCredentialProxyError> {
    cella_daemon::credential::invoke_git_credential(operation, fields).map_err(|e| {
        CellaCredentialProxyError::GitCredential {
            message: e.to_string(),
        }
    })
}
