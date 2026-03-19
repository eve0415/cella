//! Multiplexing wire protocol for tunnel communication.
//!
//! Frames are exchanged over a single bidirectional byte stream (docker exec stdin/stdout).
//! Each frame carries a channel ID, frame type, and optional payload.
//!
//! Wire format:
//! ```text
//! ┌──────────┬──────────┬──────────┬────────────┐
//! │ 4 bytes  │ 4 bytes  │ 1 byte   │ N bytes    │
//! │ length   │ channel  │ type     │ payload    │
//! │ (u32 BE) │ (u32 BE) │          │            │
//! └──────────┴──────────┴──────────┴────────────┘
//! ```
//!
//! `length` covers everything after itself: channel(4) + type(1) + payload(N) = 5 + N.

use std::io::{self, Read, Write};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum frame payload size (64 KiB).
pub const MAX_PAYLOAD_SIZE: usize = 64 * 1024;

/// Magic handshake bytes written by tunnel-server before any frames.
/// Prevents ASCII error text (e.g. "OCI runtime exec failed...") from being
/// parsed as binary mux frame headers.
pub const MAGIC: &[u8] = b"CELAMUX\x01\n";

/// Maximum bytes to scan before the magic handshake before giving up.
const MAX_PRE_MAGIC_BYTES: usize = 4096;

/// Scan for the magic handshake, returning any pre-magic bytes for logging.
///
/// Reads one byte at a time until `MAGIC` is found. Returns the bytes
/// received before the magic (useful for diagnosing error output from
/// `docker exec`).
///
/// # Errors
///
/// - `UnexpectedEof` if the stream ends before magic is found.
/// - `InvalidData` if more than `MAX_PRE_MAGIC_BYTES` are read without finding magic.
pub async fn read_magic_handshake<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let magic_len = MAGIC.len();
    let limit = MAX_PRE_MAGIC_BYTES + magic_len;

    loop {
        let mut byte = [0u8; 1];
        match reader.read_exact(&mut byte).await {
            Ok(_) => buf.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF before magic handshake",
                ));
            }
            Err(e) => return Err(e),
        }

        if buf.len() >= magic_len && buf[buf.len() - magic_len..] == *MAGIC {
            // Remove the magic suffix, return pre-magic bytes
            buf.truncate(buf.len() - magic_len);
            return Ok(buf);
        }

        if buf.len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("no magic handshake found within {MAX_PRE_MAGIC_BYTES} bytes"),
            ));
        }
    }
}

/// Frame types sent over the mux wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    /// Payload bytes for a channel.
    Data = 0x01,
    /// Open a new channel (payload: 1 byte channel kind).
    Open = 0x02,
    /// Close a channel.
    Close = 0x03,
    /// Keep-alive ping (channel = 0).
    Heartbeat = 0x04,
    /// Keep-alive pong (channel = 0).
    HeartbeatAck = 0x05,
}

impl FrameType {
    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::Data),
            0x02 => Some(Self::Open),
            0x03 => Some(Self::Close),
            0x04 => Some(Self::Heartbeat),
            0x05 => Some(Self::HeartbeatAck),
            _ => None,
        }
    }
}

/// Channel kinds, used in OPEN frame payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ChannelKind {
    /// SSH agent forwarding channel.
    SshAgent = 0x01,
    /// Git credential forwarding channel.
    Credential = 0x02,
}

impl ChannelKind {
    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::SshAgent),
            0x02 => Some(Self::Credential),
            _ => None,
        }
    }
}

/// A parsed frame.
#[derive(Debug, Clone)]
#[allow(clippy::struct_field_names)]
pub struct Frame {
    pub channel: u32,
    pub frame_type: FrameType,
    pub payload: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Sync I/O (used by tests and container binary reference)
// ---------------------------------------------------------------------------

/// Write a frame to a synchronous writer.
///
/// # Errors
///
/// Returns `io::Error` if the payload exceeds `MAX_PAYLOAD_SIZE` or a write fails.
pub fn write_frame<W: Write>(writer: &mut W, frame: &Frame) -> io::Result<()> {
    let payload_len = frame.payload.len();
    if payload_len > MAX_PAYLOAD_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("payload too large: {payload_len} > {MAX_PAYLOAD_SIZE}"),
        ));
    }

    #[allow(clippy::cast_possible_truncation)]
    let length = 5u32 + payload_len as u32;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&frame.channel.to_be_bytes())?;
    writer.write_all(&[frame.frame_type as u8])?;
    if !frame.payload.is_empty() {
        writer.write_all(&frame.payload)?;
    }
    writer.flush()
}

/// Read a single frame from a synchronous reader.
///
/// Returns `None` on clean EOF (zero-length read of the length prefix).
///
/// # Errors
///
/// Returns `io::Error` if frame data is malformed or a read fails.
pub fn read_frame<R: Read>(reader: &mut R) -> io::Result<Option<Frame>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let length = u32::from_be_bytes(len_buf) as usize;

    if length < 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame too short: length={length}"),
        ));
    }

    let payload_len = length - 5;
    if payload_len > MAX_PAYLOAD_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("payload too large: {payload_len} > {MAX_PAYLOAD_SIZE}"),
        ));
    }

    let mut chan_buf = [0u8; 4];
    reader.read_exact(&mut chan_buf)?;
    let channel = u32::from_be_bytes(chan_buf);

    let mut type_buf = [0u8; 1];
    reader.read_exact(&mut type_buf)?;
    let frame_type = FrameType::from_byte(type_buf[0]).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown frame type: 0x{:02x}", type_buf[0]),
        )
    })?;

    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        reader.read_exact(&mut payload)?;
    }

    Ok(Some(Frame {
        channel,
        frame_type,
        payload,
    }))
}

// ---------------------------------------------------------------------------
// Async I/O (used by host daemon)
// ---------------------------------------------------------------------------

/// Write a frame to an async writer.
///
/// # Errors
///
/// Returns `io::Error` if the payload exceeds `MAX_PAYLOAD_SIZE` or a write fails.
pub async fn write_frame_async<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Frame,
) -> io::Result<()> {
    let payload_len = frame.payload.len();
    if payload_len > MAX_PAYLOAD_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("payload too large: {payload_len} > {MAX_PAYLOAD_SIZE}"),
        ));
    }

    #[allow(clippy::cast_possible_truncation)]
    let length = 5u32 + payload_len as u32;
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(&frame.channel.to_be_bytes()).await?;
    writer.write_all(&[frame.frame_type as u8]).await?;
    if !frame.payload.is_empty() {
        writer.write_all(&frame.payload).await?;
    }
    writer.flush().await
}

/// Read a single frame from an async reader.
///
/// Returns `None` on clean EOF.
///
/// # Errors
///
/// Returns `io::Error` if frame data is malformed or a read fails.
pub async fn read_frame_async<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Frame>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let length = u32::from_be_bytes(len_buf) as usize;

    if length < 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame too short: length={length}"),
        ));
    }

    let payload_len = length - 5;
    if payload_len > MAX_PAYLOAD_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("payload too large: {payload_len} > {MAX_PAYLOAD_SIZE}"),
        ));
    }

    let mut chan_buf = [0u8; 4];
    reader.read_exact(&mut chan_buf).await?;
    let channel = u32::from_be_bytes(chan_buf);

    let mut type_buf = [0u8; 1];
    reader.read_exact(&mut type_buf).await?;
    let frame_type = FrameType::from_byte(type_buf[0]).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown frame type: 0x{:02x}", type_buf[0]),
        )
    })?;

    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        reader.read_exact(&mut payload).await?;
    }

    Ok(Some(Frame {
        channel,
        frame_type,
        payload,
    }))
}

// ---------------------------------------------------------------------------
// Frame constructors
// ---------------------------------------------------------------------------

pub fn open_frame(channel: u32, kind: ChannelKind) -> Frame {
    Frame {
        channel,
        frame_type: FrameType::Open,
        payload: vec![kind as u8],
    }
}

pub const fn data_frame(channel: u32, payload: Vec<u8>) -> Frame {
    Frame {
        channel,
        frame_type: FrameType::Data,
        payload,
    }
}

pub const fn close_frame(channel: u32) -> Frame {
    Frame {
        channel,
        frame_type: FrameType::Close,
        payload: Vec::new(),
    }
}

pub const fn heartbeat_frame() -> Frame {
    Frame {
        channel: 0,
        frame_type: FrameType::Heartbeat,
        payload: Vec::new(),
    }
}

pub const fn heartbeat_ack_frame() -> Frame {
    Frame {
        channel: 0,
        frame_type: FrameType::HeartbeatAck,
        payload: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip_data_frame() {
        let frame = data_frame(42, b"hello".to_vec());
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();

        let mut cursor = Cursor::new(&buf);
        let parsed = read_frame(&mut cursor).unwrap().unwrap();

        assert_eq!(parsed.channel, 42);
        assert_eq!(parsed.frame_type, FrameType::Data);
        assert_eq!(parsed.payload, b"hello");
    }

    #[test]
    fn roundtrip_open_frame() {
        let frame = open_frame(1, ChannelKind::SshAgent);
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();

        let mut cursor = Cursor::new(&buf);
        let parsed = read_frame(&mut cursor).unwrap().unwrap();

        assert_eq!(parsed.channel, 1);
        assert_eq!(parsed.frame_type, FrameType::Open);
        assert_eq!(parsed.payload, vec![ChannelKind::SshAgent as u8]);
    }

    #[test]
    fn roundtrip_close_frame() {
        let frame = close_frame(7);
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();

        let mut cursor = Cursor::new(&buf);
        let parsed = read_frame(&mut cursor).unwrap().unwrap();

        assert_eq!(parsed.channel, 7);
        assert_eq!(parsed.frame_type, FrameType::Close);
        assert!(parsed.payload.is_empty());
    }

    #[test]
    fn roundtrip_heartbeat() {
        let frame = heartbeat_frame();
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();

        let mut cursor = Cursor::new(&buf);
        let parsed = read_frame(&mut cursor).unwrap().unwrap();

        assert_eq!(parsed.channel, 0);
        assert_eq!(parsed.frame_type, FrameType::Heartbeat);
    }

    #[test]
    fn roundtrip_heartbeat_ack() {
        let frame = heartbeat_ack_frame();
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();

        let mut cursor = Cursor::new(&buf);
        let parsed = read_frame(&mut cursor).unwrap().unwrap();

        assert_eq!(parsed.channel, 0);
        assert_eq!(parsed.frame_type, FrameType::HeartbeatAck);
    }

    #[test]
    fn eof_returns_none() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let result = read_frame(&mut cursor).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn multi_channel_interleaving() {
        let frames = vec![
            open_frame(1, ChannelKind::SshAgent),
            open_frame(2, ChannelKind::Credential),
            data_frame(1, b"ssh-data".to_vec()),
            data_frame(2, b"cred-data".to_vec()),
            data_frame(1, b"more-ssh".to_vec()),
            close_frame(2),
            close_frame(1),
        ];

        let mut buf = Vec::new();
        for f in &frames {
            write_frame(&mut buf, f).unwrap();
        }

        let mut cursor = Cursor::new(&buf);
        for expected in &frames {
            let parsed = read_frame(&mut cursor).unwrap().unwrap();
            assert_eq!(parsed.channel, expected.channel);
            assert_eq!(parsed.frame_type, expected.frame_type);
            assert_eq!(parsed.payload, expected.payload);
        }

        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn rejects_oversized_payload() {
        let big = vec![0u8; MAX_PAYLOAD_SIZE + 1];
        let frame = Frame {
            channel: 1,
            frame_type: FrameType::Data,
            payload: big,
        };
        let mut buf = Vec::new();
        assert!(write_frame(&mut buf, &frame).is_err());
    }

    #[test]
    fn rejects_short_frame() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&3u32.to_be_bytes());
        buf.extend_from_slice(&[0, 0, 0]);

        let mut cursor = Cursor::new(&buf);
        assert!(read_frame(&mut cursor).is_err());
    }

    #[test]
    fn rejects_unknown_frame_type() {
        let mut buf = Vec::new();
        let length: u32 = 5;
        buf.extend_from_slice(&length.to_be_bytes());
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.push(0xFF);

        let mut cursor = Cursor::new(&buf);
        assert!(read_frame(&mut cursor).is_err());
    }

    #[test]
    fn empty_payload_data_frame() {
        let frame = data_frame(10, Vec::new());
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();

        let mut cursor = Cursor::new(&buf);
        let parsed = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(parsed.channel, 10);
        assert_eq!(parsed.frame_type, FrameType::Data);
        assert!(parsed.payload.is_empty());
    }

    #[tokio::test]
    async fn async_roundtrip_data_frame() {
        let frame = data_frame(42, b"async-hello".to_vec());
        let mut buf = Vec::new();
        write_frame_async(&mut buf, &frame).await.unwrap();

        let mut cursor = io::Cursor::new(&buf);
        let parsed = read_frame_async(&mut cursor).await.unwrap().unwrap();

        assert_eq!(parsed.channel, 42);
        assert_eq!(parsed.frame_type, FrameType::Data);
        assert_eq!(parsed.payload, b"async-hello");
    }

    #[tokio::test]
    async fn async_eof_returns_none() {
        let mut cursor = io::Cursor::new(Vec::<u8>::new());
        let result = read_frame_async(&mut cursor).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn magic_found_immediately() {
        let mut cursor = io::Cursor::new(MAGIC.to_vec());
        let pre_magic = read_magic_handshake(&mut cursor).await.unwrap();
        assert!(pre_magic.is_empty());
    }

    #[tokio::test]
    async fn magic_found_after_preamble() {
        let mut data = b"OCI runtime exec failed\n".to_vec();
        data.extend_from_slice(MAGIC);
        let mut cursor = io::Cursor::new(data);
        let pre_magic = read_magic_handshake(&mut cursor).await.unwrap();
        assert_eq!(pre_magic, b"OCI runtime exec failed\n");
    }

    #[tokio::test]
    async fn magic_not_found_eof() {
        let mut cursor = io::Cursor::new(b"partial data".to_vec());
        let result = read_magic_handshake(&mut cursor).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn magic_not_found_limit() {
        let noise = vec![0xAA; MAX_PRE_MAGIC_BYTES + MAGIC.len() + 100];
        let mut cursor = io::Cursor::new(noise);
        let result = read_magic_handshake(&mut cursor).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
