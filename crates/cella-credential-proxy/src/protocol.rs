//! Git credential wire protocol — re-exported from `cella-daemon`.
//!
//! The canonical implementation lives in `cella_daemon::credential`.
//! This module re-exports the types and wraps error-returning functions
//! to map `CellaDaemonError` to `CellaCredentialProxyError`.

pub use cella_daemon::credential::{
    CredentialRequest, CredentialResponse, format_fields_for_stdin, format_response,
    parse_credential_output,
};

use crate::CellaCredentialProxyError;

/// Parse a credential request, mapping daemon errors to proxy errors.
///
/// # Errors
///
/// Returns `CellaCredentialProxyError::Protocol` if the request is empty or malformed.
pub fn parse_request(data: &str) -> Result<CredentialRequest, CellaCredentialProxyError> {
    cella_daemon::credential::parse_request(data).map_err(|e| CellaCredentialProxyError::Protocol {
        message: e.to_string(),
    })
}
