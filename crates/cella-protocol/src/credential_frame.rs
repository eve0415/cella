//! Binary frame protocol types for the multiplexed credential tunnel.
//!
//! After the `0x03` magic byte selects the credential connection type, all
//! subsequent bytes on the stream are framed messages. Each frame has a 9-byte
//! header (`request_id` + `frame_type` + `payload_len`, all big-endian) followed by a
//! variable-length payload.
//!
//! Multiple concurrent requests are multiplexed over a single TCP connection
//! using the `request_id` field.

use std::fmt;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Magic byte constants
// ---------------------------------------------------------------------------

/// First byte on a new TCP connection: agent control (JSON `AgentHello`).
pub const MAGIC_AGENT_HELLO: u8 = 0x01;

/// First byte on a new TCP connection: reverse tunnel for port forwarding.
pub const MAGIC_TUNNEL: u8 = 0x02;

/// First byte on a new TCP connection: multiplexed credential proxy.
pub const MAGIC_CREDENTIAL: u8 = 0x03;

// ---------------------------------------------------------------------------
// Frame header size
// ---------------------------------------------------------------------------

/// Total size of a serialized frame header in bytes.
pub const FRAME_HEADER_SIZE: usize = 9;

// ---------------------------------------------------------------------------
// Resource limits (compile-time, not user-configurable)
// ---------------------------------------------------------------------------

/// Maximum payload for `Handshake` frames (8 KB).
pub const MAX_HANDSHAKE_PAYLOAD: u32 = 8 * 1024;

/// Maximum payload for `RequestEnvelope` and `ResponseEnvelope` frames (8 MB).
pub const MAX_ENVELOPE_PAYLOAD: u32 = 8 * 1024 * 1024;

/// Maximum payload for a single `RequestChunk` frame (16 MB).
pub const MAX_REQUEST_CHUNK: u32 = 16 * 1024 * 1024;

/// Maximum payload for a single `ResponseChunk` frame (16 MB).
pub const MAX_RESPONSE_CHUNK: u32 = 16 * 1024 * 1024;

/// Maximum total request body across all chunks (256 MB).
pub const MAX_REQUEST_BODY: u64 = 256 * 1024 * 1024;

/// Maximum total response body across all chunks (1 GB).
pub const MAX_RESPONSE_TOTAL: u64 = 1024 * 1024 * 1024;

/// Maximum payload for `Error` frames (8 KB).
pub const MAX_ERROR_PAYLOAD: u32 = 8 * 1024;

/// Maximum number of in-flight requests per multiplexed connection.
pub const MAX_CONCURRENT_REQUESTS: usize = 64;

/// Maximum number of headers in a request or response envelope.
pub const MAX_HEADERS: usize = 100;

/// Maximum size of a single serialized header key+value pair (64 KB).
pub const MAX_HEADER_PAIR: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Frame type
// ---------------------------------------------------------------------------

/// Discriminator for credential tunnel frame types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FrameType {
    /// Agent -> daemon: initiate a new proxied request.
    Handshake = 0x01,
    /// Agent -> daemon: HTTP request metadata (method, URI, headers).
    RequestEnvelope = 0x02,
    /// Agent -> daemon: raw request body chunk.
    RequestChunk = 0x03,
    /// Agent -> daemon: end of request body (payload must be empty).
    RequestEnd = 0x04,
    /// Daemon -> agent: HTTP response metadata (status, headers).
    ResponseEnvelope = 0x05,
    /// Daemon -> agent: raw response body chunk.
    ResponseChunk = 0x06,
    /// Daemon -> agent: end of response body (payload must be empty).
    ResponseEnd = 0x07,
    /// Daemon -> agent: request-scoped error.
    Error = 0x08,
    /// Agent -> daemon: cancel an in-flight request (payload must be empty).
    Cancel = 0x09,
}

/// Error returned when a byte does not map to a known [`FrameType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidFrameType(pub u8);

impl fmt::Display for InvalidFrameType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid frame type: {:#04x}", self.0)
    }
}

impl std::error::Error for InvalidFrameType {}

impl TryFrom<u8> for FrameType {
    type Error = InvalidFrameType;

    fn try_from(value: u8) -> Result<Self, <Self as TryFrom<u8>>::Error> {
        match value {
            0x01 => Ok(Self::Handshake),
            0x02 => Ok(Self::RequestEnvelope),
            0x03 => Ok(Self::RequestChunk),
            0x04 => Ok(Self::RequestEnd),
            0x05 => Ok(Self::ResponseEnvelope),
            0x06 => Ok(Self::ResponseChunk),
            0x07 => Ok(Self::ResponseEnd),
            0x08 => Ok(Self::Error),
            0x09 => Ok(Self::Cancel),
            _ => Err(InvalidFrameType(value)),
        }
    }
}

impl fmt::Display for FrameType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Handshake => "Handshake",
            Self::RequestEnvelope => "RequestEnvelope",
            Self::RequestChunk => "RequestChunk",
            Self::RequestEnd => "RequestEnd",
            Self::ResponseEnvelope => "ResponseEnvelope",
            Self::ResponseChunk => "ResponseChunk",
            Self::ResponseEnd => "ResponseEnd",
            Self::Error => "Error",
            Self::Cancel => "Cancel",
        };
        f.write_str(name)
    }
}

// ---------------------------------------------------------------------------
// Frame header
// ---------------------------------------------------------------------------

/// 9-byte frame header: `request_id (4B) | frame_type (1B) | payload_len (4B)`.
///
/// All multi-byte integers are big-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Multiplexing key: routes frames to the correct request handler.
    pub request_id: u32,
    /// Discriminator for the frame payload.
    pub frame_type: FrameType,
    /// Length of the payload that follows this header.
    pub payload_len: u32,
}

impl FrameHeader {
    /// Serialize the header into a 9-byte big-endian array.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; FRAME_HEADER_SIZE] {
        let mut buf = [0u8; FRAME_HEADER_SIZE];
        buf[..4].copy_from_slice(&self.request_id.to_be_bytes());
        buf[4] = self.frame_type as u8;
        buf[5..9].copy_from_slice(&self.payload_len.to_be_bytes());
        buf
    }

    /// Deserialize a header from a 9-byte big-endian array.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidFrameType`] if the frame type byte is unknown.
    pub fn from_bytes(buf: &[u8; FRAME_HEADER_SIZE]) -> Result<Self, InvalidFrameType> {
        let request_id = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let frame_type = FrameType::try_from(buf[4])?;
        let payload_len = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]);
        Ok(Self {
            request_id,
            frame_type,
            payload_len,
        })
    }
}

// ---------------------------------------------------------------------------
// I/O helpers (synchronous, no tokio dependency)
// ---------------------------------------------------------------------------

/// Read a frame header from a synchronous reader.
///
/// Validates that `payload_len` does not exceed the frame-type-specific
/// maximum before returning. This ensures receivers reject oversized frames
/// **before allocating memory** for the payload.
///
/// # Errors
///
/// Returns an I/O error if reading fails, the frame type byte is unknown,
/// or `payload_len` exceeds the frame-type-specific limit.
pub fn read_frame_header<R: Read>(reader: &mut R) -> io::Result<FrameHeader> {
    let mut buf = [0u8; FRAME_HEADER_SIZE];
    reader.read_exact(&mut buf)?;

    let header =
        FrameHeader::from_bytes(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let max = max_payload_for_frame_type(header.frame_type);
    if header.payload_len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "payload_len {} exceeds maximum {} for frame type {}",
                header.payload_len, max, header.frame_type,
            ),
        ));
    }

    Ok(header)
}

/// Write a complete frame (header + payload) to a synchronous writer.
///
/// Does **not** validate payload length against frame-type limits; the caller
/// is responsible for respecting the limits when constructing outgoing frames.
///
/// # Errors
///
/// Returns an I/O error if writing fails or the payload length exceeds
/// `u32::MAX`.
pub fn write_frame<W: Write>(
    writer: &mut W,
    request_id: u32,
    frame_type: FrameType,
    payload: &[u8],
) -> io::Result<()> {
    let header = FrameHeader {
        request_id,
        frame_type,
        payload_len: u32::try_from(payload.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "payload exceeds u32::MAX bytes",
            )
        })?,
    };
    writer.write_all(&header.to_bytes())?;
    if !payload.is_empty() {
        writer.write_all(payload)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Payload size limit helper
// ---------------------------------------------------------------------------

/// Returns the maximum allowed `payload_len` for a given frame type.
///
/// `RequestEnd`, `ResponseEnd`, and `Cancel` must have empty payloads (max = 0).
#[must_use]
pub const fn max_payload_for_frame_type(frame_type: FrameType) -> u32 {
    match frame_type {
        FrameType::Handshake => MAX_HANDSHAKE_PAYLOAD,
        FrameType::RequestEnvelope | FrameType::ResponseEnvelope => MAX_ENVELOPE_PAYLOAD,
        FrameType::RequestChunk => MAX_REQUEST_CHUNK,
        FrameType::ResponseChunk => MAX_RESPONSE_CHUNK,
        FrameType::Error => MAX_ERROR_PAYLOAD,
        FrameType::RequestEnd | FrameType::ResponseEnd | FrameType::Cancel => 0,
    }
}

// ---------------------------------------------------------------------------
// Error envelope
// ---------------------------------------------------------------------------

/// Payload of an `Error` frame, serialized as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    /// Error category (see [`error_category`] constants).
    pub category: String,
    /// Human-readable diagnostic message. Never contains credentials.
    pub message: String,
    /// Trace ID from the handshake, for audit correlation.
    /// Absent for pre-handshake errors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Error category constants
// ---------------------------------------------------------------------------

/// Well-known error category strings for [`ErrorEnvelope`].
pub mod error_category {
    /// Phantom token not found in registry or not present in request.
    pub const TOKEN_INVALID: &str = "token_invalid";
    /// Handshake `provider_id` does not match the token's registered provider.
    pub const PROVIDER_MISMATCH: &str = "provider_mismatch";
    /// Request domain is not in the provider's registered domain list.
    pub const DOMAIN_UNREGISTERED: &str = "domain_unregistered";
    /// Real credential could not be resolved.
    pub const CREDENTIAL_UNAVAILABLE: &str = "credential_unavailable";
    /// Upstream API request failed (DNS, TLS, timeout, connection refused).
    pub const UPSTREAM_ERROR: &str = "upstream_error";
    /// Malformed envelope, invalid frame, or unexpected frame sequence.
    pub const PROTOCOL_VIOLATION: &str = "protocol_violation";
    /// Request or response body exceeded size limit.
    pub const BODY_EXCEEDED: &str = "body_exceeded";
    /// Per-phase or total timeout exceeded.
    pub const TIMEOUT: &str = "timeout";
    /// Container nonce does not match the registered nonce.
    pub const NONCE_INVALID: &str = "nonce_invalid";
    /// Concurrent request limit exceeded on this connection.
    pub const TOO_MANY_REQUESTS: &str = "too_many_requests";
    /// Daemon is shutting down or the tunnel connection is being closed.
    pub const CONNECTION_CLOSING: &str = "connection_closing";
}

// ---------------------------------------------------------------------------
// Per-request state machine
// ---------------------------------------------------------------------------

/// States of a single credential proxy request within the multiplexed
/// connection. See the spec's "Per-request state machine" table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestState {
    /// Initial state. Expects `Handshake` frame.
    Init,
    /// Handshake validated. Expects `RequestEnvelope`.
    Handshaken,
    /// Envelope received and validated. Expects `RequestChunk`, `RequestEnd`, or `Cancel`.
    EnvelopeReceived,
    /// At least one `RequestChunk` received. Expects more chunks, `RequestEnd`, or `Cancel`.
    Streaming,
    /// `RequestEnd` sent; waiting for daemon to produce the response.
    AwaitingResponse,
    /// `ResponseEnvelope` sent; streaming response body.
    Responding,
    /// Request complete (success, error, or cancel). ID is released.
    Terminal,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    // -- Magic byte constants -------------------------------------------------

    #[test]
    fn magic_bytes_have_correct_values() {
        assert_eq!(MAGIC_AGENT_HELLO, 0x01);
        assert_eq!(MAGIC_TUNNEL, 0x02);
        assert_eq!(MAGIC_CREDENTIAL, 0x03);
    }

    // -- FrameType TryFrom ----------------------------------------------------

    #[test]
    fn frame_type_try_from_all_valid() {
        let expected = [
            (0x01, FrameType::Handshake),
            (0x02, FrameType::RequestEnvelope),
            (0x03, FrameType::RequestChunk),
            (0x04, FrameType::RequestEnd),
            (0x05, FrameType::ResponseEnvelope),
            (0x06, FrameType::ResponseChunk),
            (0x07, FrameType::ResponseEnd),
            (0x08, FrameType::Error),
            (0x09, FrameType::Cancel),
        ];
        for (byte, variant) in expected {
            assert_eq!(FrameType::try_from(byte).unwrap(), variant);
        }
    }

    #[test]
    fn frame_type_try_from_invalid_returns_error() {
        let err = FrameType::try_from(0xFF).unwrap_err();
        assert_eq!(err, InvalidFrameType(0xFF));
        assert!(err.to_string().contains("0xff"));
    }

    #[test]
    fn frame_type_try_from_zero_is_invalid() {
        assert!(FrameType::try_from(0x00).is_err());
    }

    #[test]
    fn frame_type_try_from_above_range_is_invalid() {
        assert!(FrameType::try_from(0x0A).is_err());
    }

    // -- FrameHeader bytes roundtrip ------------------------------------------

    #[test]
    fn frame_header_to_bytes_and_back() {
        let header = FrameHeader {
            request_id: 42,
            frame_type: FrameType::RequestEnvelope,
            payload_len: 1024,
        };
        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), FRAME_HEADER_SIZE);
        let decoded = FrameHeader::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, header);
    }

    #[test]
    fn frame_header_big_endian_encoding() {
        let header = FrameHeader {
            request_id: 0x0102_0304,
            frame_type: FrameType::Handshake,
            payload_len: 0x0506_0708,
        };
        let bytes = header.to_bytes();
        assert_eq!(bytes[0], 0x01);
        assert_eq!(bytes[1], 0x02);
        assert_eq!(bytes[2], 0x03);
        assert_eq!(bytes[3], 0x04);
        assert_eq!(bytes[4], 0x01); // Handshake
        assert_eq!(bytes[5], 0x05);
        assert_eq!(bytes[6], 0x06);
        assert_eq!(bytes[7], 0x07);
        assert_eq!(bytes[8], 0x08);
    }

    #[test]
    fn frame_header_from_bytes_invalid_frame_type() {
        let mut bytes = [0u8; FRAME_HEADER_SIZE];
        bytes[4] = 0xFF;
        assert!(FrameHeader::from_bytes(&bytes).is_err());
    }

    // -- Read/write roundtrip via Cursor --------------------------------------

    #[test]
    fn read_write_frame_roundtrip() {
        let payload = b"hello world";
        let mut buf = Vec::new();
        write_frame(&mut buf, 7, FrameType::RequestChunk, payload).unwrap();

        let mut reader = Cursor::new(&buf);
        let header = read_frame_header(&mut reader).unwrap();
        assert_eq!(header.request_id, 7);
        assert_eq!(header.frame_type, FrameType::RequestChunk);
        assert_eq!(header.payload_len, 11);

        let mut body = vec![0u8; header.payload_len as usize];
        reader.read_exact(&mut body).unwrap();
        assert_eq!(&body, payload);
    }

    #[test]
    fn read_write_empty_payload_frames() {
        for frame_type in [
            FrameType::RequestEnd,
            FrameType::ResponseEnd,
            FrameType::Cancel,
        ] {
            let mut buf = Vec::new();
            write_frame(&mut buf, 99, frame_type, &[]).unwrap();

            let mut reader = Cursor::new(&buf);
            let header = read_frame_header(&mut reader).unwrap();
            assert_eq!(header.request_id, 99);
            assert_eq!(header.frame_type, frame_type);
            assert_eq!(header.payload_len, 0);
        }
    }

    #[test]
    fn read_frame_header_rejects_oversized_payload() {
        let header = FrameHeader {
            request_id: 1,
            frame_type: FrameType::Handshake,
            payload_len: MAX_HANDSHAKE_PAYLOAD + 1,
        };
        let bytes = header.to_bytes();
        let mut reader = Cursor::new(&bytes);
        let err = read_frame_header(&mut reader).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_frame_header_rejects_nonempty_request_end() {
        let header = FrameHeader {
            request_id: 1,
            frame_type: FrameType::RequestEnd,
            payload_len: 1,
        };
        let bytes = header.to_bytes();
        let mut reader = Cursor::new(&bytes);
        let err = read_frame_header(&mut reader).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_frame_header_rejects_nonempty_response_end() {
        let header = FrameHeader {
            request_id: 1,
            frame_type: FrameType::ResponseEnd,
            payload_len: 1,
        };
        let bytes = header.to_bytes();
        let mut reader = Cursor::new(&bytes);
        assert!(read_frame_header(&mut reader).is_err());
    }

    #[test]
    fn read_frame_header_rejects_nonempty_cancel() {
        let header = FrameHeader {
            request_id: 1,
            frame_type: FrameType::Cancel,
            payload_len: 1,
        };
        let bytes = header.to_bytes();
        let mut reader = Cursor::new(&bytes);
        assert!(read_frame_header(&mut reader).is_err());
    }

    // -- max_payload_for_frame_type -------------------------------------------

    #[test]
    fn max_payload_limits_are_correct() {
        assert_eq!(
            max_payload_for_frame_type(FrameType::Handshake),
            MAX_HANDSHAKE_PAYLOAD
        );
        assert_eq!(
            max_payload_for_frame_type(FrameType::RequestEnvelope),
            MAX_ENVELOPE_PAYLOAD
        );
        assert_eq!(
            max_payload_for_frame_type(FrameType::RequestChunk),
            MAX_REQUEST_CHUNK
        );
        assert_eq!(
            max_payload_for_frame_type(FrameType::ResponseEnvelope),
            MAX_ENVELOPE_PAYLOAD
        );
        assert_eq!(
            max_payload_for_frame_type(FrameType::ResponseChunk),
            MAX_RESPONSE_CHUNK
        );
        assert_eq!(
            max_payload_for_frame_type(FrameType::Error),
            MAX_ERROR_PAYLOAD
        );
        assert_eq!(max_payload_for_frame_type(FrameType::RequestEnd), 0);
        assert_eq!(max_payload_for_frame_type(FrameType::ResponseEnd), 0);
        assert_eq!(max_payload_for_frame_type(FrameType::Cancel), 0);
    }

    // -- ErrorEnvelope serde --------------------------------------------------

    #[test]
    fn error_envelope_serialization_roundtrip() {
        let envelope = ErrorEnvelope {
            category: error_category::TOKEN_INVALID.to_string(),
            message: "Phantom token not found in registry".to_string(),
            trace_id: Some("cred-550e8400".to_string()),
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let decoded: ErrorEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.category, error_category::TOKEN_INVALID);
        assert_eq!(decoded.trace_id.as_deref(), Some("cred-550e8400"));
    }

    #[test]
    fn error_envelope_omits_none_trace_id() {
        let envelope = ErrorEnvelope {
            category: error_category::PROTOCOL_VIOLATION.to_string(),
            message: "bad frame".to_string(),
            trace_id: None,
        };
        let json = serde_json::to_string(&envelope).unwrap();
        assert!(!json.contains("trace_id"));
        let decoded: ErrorEnvelope = serde_json::from_str(&json).unwrap();
        assert!(decoded.trace_id.is_none());
    }

    // -- FrameType Display ----------------------------------------------------

    #[test]
    fn frame_type_display() {
        assert_eq!(FrameType::Handshake.to_string(), "Handshake");
        assert_eq!(FrameType::Cancel.to_string(), "Cancel");
    }

    // -- InvalidFrameType Display ---------------------------------------------

    #[test]
    fn invalid_frame_type_display() {
        let err = InvalidFrameType(0x42);
        assert_eq!(err.to_string(), "invalid frame type: 0x42");
    }

    // -- Multiple frames on one stream ----------------------------------------

    #[test]
    fn multiple_frames_on_one_stream() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 1, FrameType::Handshake, b"{}").unwrap();
        write_frame(
            &mut buf,
            1,
            FrameType::RequestEnvelope,
            b"{\"method\":\"GET\"}",
        )
        .unwrap();
        write_frame(&mut buf, 1, FrameType::RequestEnd, &[]).unwrap();

        let mut reader = Cursor::new(&buf);

        let h1 = read_frame_header(&mut reader).unwrap();
        assert_eq!(h1.frame_type, FrameType::Handshake);
        let mut p1 = vec![0u8; h1.payload_len as usize];
        reader.read_exact(&mut p1).unwrap();

        let h2 = read_frame_header(&mut reader).unwrap();
        assert_eq!(h2.frame_type, FrameType::RequestEnvelope);
        let mut p2 = vec![0u8; h2.payload_len as usize];
        reader.read_exact(&mut p2).unwrap();

        let h3 = read_frame_header(&mut reader).unwrap();
        assert_eq!(h3.frame_type, FrameType::RequestEnd);
        assert_eq!(h3.payload_len, 0);
    }
}
