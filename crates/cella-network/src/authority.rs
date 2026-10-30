//! Parsing of request-target authorities (`host[:port]`) as the agent proxy
//! sees them, shared with `cella network test` so both evaluate rules
//! against the same canonical host.

use std::fmt;
use std::net::IpAddr;

use crate::rules::canonical_ipv6_literal;

/// Default port of an absolute-form `http://` target.
pub const HTTP_DEFAULT_PORT: u16 = 80;

/// Default port of a CONNECT target.
pub const CONNECT_DEFAULT_PORT: u16 = 443;

/// A parsed `host[:port]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPort<'a> {
    /// Lowercased host for rule matching, credential lookup, the pool key,
    /// and connecting. A bracketed IPv6 literal is in canonical form without
    /// brackets, or dotted IPv4 when it is IPv4-mapped (`::ffff:a.b.c.d`).
    pub host: String,
    /// Host as written, with an IPv6 literal in brackets.
    pub written: &'a str,
    pub port: u16,
}

/// Why an authority was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityError {
    /// The host is empty.
    EmptyHost,
    /// A port is given but is not a decimal `u16`.
    InvalidPort,
    /// An unterminated bracket, text after `]` other than a port, or an
    /// unbracketed host containing `:`.
    Malformed,
    /// A bracketed host is not an IPv6 address (a zone ID included).
    InvalidIpv6Literal,
    /// The host is the unspecified address, which connects to this machine
    /// whatever the rules say about loopback.
    UnspecifiedAddress,
    /// A CONNECT target carries userinfo.
    Userinfo,
}

impl fmt::Display for AuthorityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyHost => "empty host",
            Self::InvalidPort => "port is not a number from 0 to 65535",
            Self::Malformed => "malformed host:port",
            Self::InvalidIpv6Literal => "bracketed host is not an IPv6 address",
            Self::UnspecifiedAddress => "unspecified address (0.0.0.0 or ::)",
            Self::Userinfo => "CONNECT target carries userinfo",
        })
    }
}

impl std::error::Error for AuthorityError {}

/// Parse a CONNECT target, defaulting to port 443.
///
/// A target with userinfo is rejected: it would be matched as part of the
/// host and must not reach the log.
///
/// # Errors
///
/// Returns [`AuthorityError::Userinfo`] for a target containing `@`, and
/// otherwise whatever [`parse_authority`] returns.
pub fn parse_connect_target(target: &str) -> Result<HostPort<'_>, AuthorityError> {
    if target.contains('@') {
        return Err(AuthorityError::Userinfo);
    }
    parse_authority(target, CONNECT_DEFAULT_PORT)
}

/// Parse the authority of an absolute-form `http://` target, defaulting to
/// port 80.
///
/// Any userinfo is dropped: only the host and port are kept.
///
/// # Errors
///
/// Returns whatever [`parse_authority`] returns for the text after the last
/// `@`.
pub fn parse_plain_authority(authority: &str) -> Result<HostPort<'_>, AuthorityError> {
    let host_and_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    parse_authority(host_and_port, HTTP_DEFAULT_PORT)
}

/// Parse `host[:port]`, defaulting the port to `default_port` when none is
/// given or the port after `:` is empty (RFC 3986 §3.2.3).
///
/// # Errors
///
/// Returns an error when the host is empty, a port is given but is not a
/// decimal `u16`, the text is not `host[:port]`, a bracketed host is not an
/// IPv6 address (a zone ID included), or the host is the unspecified address
/// (`0.0.0.0`, `::`, or `::ffff:0.0.0.0`).
pub fn parse_authority(authority: &str, default_port: u16) -> Result<HostPort<'_>, AuthorityError> {
    let (written, port) = split_host_port(authority).ok_or(AuthorityError::Malformed)?;
    if written.is_empty() {
        return Err(AuthorityError::EmptyHost);
    }
    let port = match port {
        None | Some("") => default_port,
        Some(port) if port.bytes().all(|b| b.is_ascii_digit()) => {
            port.parse().map_err(|_| AuthorityError::InvalidPort)?
        }
        Some(_) => return Err(AuthorityError::InvalidPort),
    };
    let host = if written.starts_with('[') {
        canonical_ipv6_literal(written).ok_or(AuthorityError::InvalidIpv6Literal)?
    } else {
        written.to_ascii_lowercase()
    };
    if host.parse::<IpAddr>().is_ok_and(|ip| ip.is_unspecified()) {
        return Err(AuthorityError::UnspecifiedAddress);
    }
    Ok(HostPort {
        host,
        written,
        port,
    })
}

/// Split `host[:port]` into the host as written, IPv6 brackets included,
/// and the port text when one is present.
///
/// Returns `None` for an unterminated bracket, text after `]` other than a
/// port, or an unbracketed host containing `:`.
fn split_host_port(authority: &str) -> Option<(&str, Option<&str>)> {
    if authority.starts_with('[') {
        let (host, after) = authority.split_at(authority.find(']')? + 1);
        let port = if after.is_empty() {
            None
        } else {
            Some(after.strip_prefix(':')?)
        };
        return Some((host, port));
    }
    match authority.split_once(':') {
        Some((_, port)) if port.contains(':') => None,
        Some((host, port)) => Some((host, Some(port))),
        None => Some((authority, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connect(target: &str) -> Result<(String, u16), AuthorityError> {
        parse_connect_target(target).map(|hp| (hp.host, hp.port))
    }

    fn ok(host: &str, port: u16) -> (String, u16) {
        (host.to_string(), port)
    }

    #[test]
    fn parse_connect_target_with_port() {
        assert_eq!(connect("example.com:443"), Ok(ok("example.com", 443)));
    }

    #[test]
    fn parse_connect_target_no_port() {
        assert_eq!(connect("example.com"), Ok(ok("example.com", 443)));
    }

    #[test]
    fn parse_connect_target_non_standard_port() {
        assert_eq!(connect("myhost:8443"), Ok(ok("myhost", 8443)));
    }

    #[test]
    fn parse_connect_target_ipv6_literal() {
        assert_eq!(connect("[::1]:443"), Ok(ok("::1", 443)));
        assert_eq!(connect("[2001:DB8::1]:8443"), Ok(ok("2001:db8::1", 8443)));
        assert_eq!(connect("[0:0:0:0:0:0:0:1]:443"), Ok(ok("::1", 443)));
        assert_eq!(connect("[::ffff:127.0.0.1]:443"), Ok(ok("127.0.0.1", 443)));
        assert_eq!(connect("[::127.0.0.1]:443"), Ok(ok("::7f00:1", 443)));
        assert_eq!(
            connect("[::1%25eth0]:443"),
            Err(AuthorityError::InvalidIpv6Literal)
        );
        assert_eq!(
            connect("[not-an-ip]:443"),
            Err(AuthorityError::InvalidIpv6Literal)
        );
        assert_eq!(connect("[::1]"), Ok(ok("::1", 443)));
        assert_eq!(connect("[::1"), Err(AuthorityError::Malformed));
        assert_eq!(connect("[::1]443"), Err(AuthorityError::Malformed));
        assert_eq!(connect("::1:443"), Err(AuthorityError::Malformed));
    }

    #[test]
    fn written_keeps_brackets_and_case() {
        let hp = parse_plain_authority("[::1]:8080").unwrap();
        assert_eq!(
            (hp.host.as_str(), hp.written, hp.port),
            ("::1", "[::1]", 8080)
        );
        let hp = parse_plain_authority("Example.com").unwrap();
        assert_eq!(
            (hp.host.as_str(), hp.written, hp.port),
            ("example.com", "Example.com", 80)
        );
    }

    #[test]
    fn empty_port_means_default() {
        assert_eq!(connect("example.com:"), Ok(ok("example.com", 443)));
        assert_eq!(connect("[::1]:"), Ok(ok("::1", 443)));
        assert_eq!(parse_plain_authority("[::1]:").map(|hp| hp.port), Ok(80));
    }

    #[test]
    fn invalid_port_is_rejected() {
        for target in ["myhost:notaport", "myhost:99999", "example.com:+443"] {
            assert_eq!(
                connect(target),
                Err(AuthorityError::InvalidPort),
                "{target}"
            );
        }
        for authority in ["example.com:65536", "example.com:abc", "[::1]:abc"] {
            assert_eq!(
                parse_plain_authority(authority),
                Err(AuthorityError::InvalidPort),
                "{authority}"
            );
        }
    }

    #[test]
    fn unspecified_address_is_rejected() {
        for target in ["[::]", "[0::0]", "[::ffff:0.0.0.0]", "0.0.0.0"] {
            assert_eq!(
                connect(&format!("{target}:443")),
                Err(AuthorityError::UnspecifiedAddress),
                "{target}"
            );
            assert_eq!(
                parse_plain_authority(&format!("{target}:80")),
                Err(AuthorityError::UnspecifiedAddress),
                "{target}"
            );
        }
    }

    #[test]
    fn empty_host_is_rejected() {
        for target in [":443", ":", ""] {
            assert_eq!(
                connect(target),
                Err(AuthorityError::EmptyHost),
                "{target:?}"
            );
        }
        assert_eq!(
            parse_plain_authority(":8080"),
            Err(AuthorityError::EmptyHost)
        );
    }

    #[test]
    fn connect_target_rejects_userinfo() {
        assert_eq!(
            connect("u:secret@example.com:443"),
            Err(AuthorityError::Userinfo)
        );
        assert_eq!(connect("u@example.com"), Err(AuthorityError::Userinfo));
    }

    #[test]
    fn plain_authority_drops_userinfo() {
        let hp = parse_plain_authority("user:pw@example.com").unwrap();
        assert_eq!((hp.host.as_str(), hp.port), ("example.com", 80));
    }
}
