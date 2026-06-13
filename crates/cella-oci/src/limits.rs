//! Blob size caps to defend against zip-bombs and oversized blobs.
//!
//! [`LimitedReader`] wraps any [`Read`] and returns a hard [`io::Error`] when
//! the byte count exceeds the configured limit — it never silently truncates.
//!
//! [`LimitedWriter`] wraps any [`tokio::io::AsyncWrite`] and returns a hard
//! error when bytes written exceed the configured limit — used when streaming
//! blobs via `pull_blob`.

use std::io::{self, Read};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::AsyncWrite;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum compressed (downloaded) blob size: 512 MiB.
pub const MAX_BLOB_COMPRESSED_BYTES: u64 = 512 * 1024 * 1024;

/// Maximum decompressed output size: 2 GiB.
pub const MAX_BLOB_DECOMPRESSED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Maximum size for collection/index JSON: 64 MiB.
pub const MAX_COLLECTION_JSON_BYTES: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Streaming accumulation guard
// ---------------------------------------------------------------------------

/// Returns `true` when appending a chunk of `chunk_len` bytes to a buffer that
/// already holds `current_len` bytes would push the total past `cap`.
///
/// Call this *before* extending the buffer so a single oversized chunk cannot
/// force an allocation past the cap.  Saturating arithmetic keeps the check
/// sound even if the lengths cannot be represented exactly.
#[must_use]
pub fn would_exceed_cap(current_len: usize, chunk_len: usize, cap: u64) -> bool {
    let projected = u64::try_from(current_len)
        .unwrap_or(u64::MAX)
        .saturating_add(u64::try_from(chunk_len).unwrap_or(u64::MAX));
    projected > cap
}

// ---------------------------------------------------------------------------
// Typed limit error
// ---------------------------------------------------------------------------

/// Marker error carried inside the [`io::Error`] that [`LimitedReader`] emits
/// when the decompressed byte count exceeds its limit.
///
/// Callers can recover the limit from a bubbled-up [`io::Error`] via
/// [`limit_from_io_error`] and surface a typed, non-`Io` diagnostic (e.g.
/// `ExtractionError::DecompressedTooLarge`) instead of an opaque I/O failure.
#[derive(Debug)]
pub struct LimitExceeded {
    /// The configured byte cap that was exceeded.
    pub limit: u64,
}

impl std::fmt::Display for LimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "output exceeds limit of {} bytes", self.limit)
    }
}

impl std::error::Error for LimitExceeded {}

impl LimitExceeded {
    /// Wrap this marker in an [`io::Error`] of kind [`io::ErrorKind::Other`].
    fn into_io_error(self) -> io::Error {
        io::Error::other(self)
    }
}

/// If `err` (or any error nested inside it by the `tar`/`flate2` stack)
/// originated from a [`LimitedReader`] hitting its cap, return the cap.
///
/// The `tar` crate re-wraps reader failures as `io::Error -> TarError ->
/// io::Error -> … -> LimitExceeded`.  A plain `Error::source()` walk misses
/// the [`LimitExceeded`] payload because each `io::Error` stores its inner
/// error in `get_ref()`, not in `source()`.  This walk descends through both.
///
/// Returns `None` for unrelated I/O errors.
#[must_use]
pub fn limit_from_io_error(err: &io::Error) -> Option<u64> {
    fn descend(err: &(dyn std::error::Error + 'static)) -> Option<u64> {
        if let Some(exceeded) = err.downcast_ref::<LimitExceeded>() {
            return Some(exceeded.limit);
        }
        // An inner `io::Error` keeps its payload in `get_ref()`.
        if let Some(io_err) = err.downcast_ref::<io::Error>()
            && let Some(inner) = io_err.get_ref()
            && let Some(found) = descend(inner)
        {
            return Some(found);
        }
        // Fall back to the standard source chain (e.g. `TarError`).
        err.source().and_then(descend)
    }

    let inner = err.get_ref()?;
    descend(inner)
}

// ---------------------------------------------------------------------------
// LimitedReader
// ---------------------------------------------------------------------------

/// A [`Read`] adaptor that returns an error when the total bytes read exceeds
/// `limit`.
///
/// Unlike [`std::io::Take`], this returns `Err` instead of EOF so callers
/// discover the violation rather than silently reading a truncated stream.
///
/// When the underlying stream contains **exactly** `limit` bytes the reader
/// returns `Ok(0)` (EOF) rather than an error.  An error is only returned when
/// the stream contains *more* than `limit` bytes.
pub struct LimitedReader<R> {
    inner: R,
    limit: u64,
    consumed: u64,
    /// Set to `true` once we have confirmed that the inner stream is exhausted
    /// at exactly the limit boundary (peek returned 0 bytes).
    at_eof: bool,
}

impl<R: Read> LimitedReader<R> {
    /// Wrap `inner`, refusing to deliver more than `limit` bytes in total.
    pub const fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            limit,
            consumed: 0,
            at_eof: false,
        }
    }
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Already confirmed EOF at the limit boundary.
        if self.at_eof {
            return Ok(0);
        }

        let remaining = self.limit.saturating_sub(self.consumed);
        if remaining == 0 {
            // We're at the limit; peek to decide: EOF → Ok(0), more data → Err.
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe) {
                Ok(0) => {
                    self.at_eof = true;
                    Ok(0)
                }
                Ok(_) => Err(LimitExceeded { limit: self.limit }.into_io_error()),
                Err(e) => Err(e),
            };
        }

        // Only ask the inner reader for `remaining` bytes at most.
        let capped_len = buf
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..capped_len])?;
        self.consumed = self
            .consumed
            .saturating_add(u64::try_from(n).unwrap_or(u64::MAX));

        // If we've now consumed exactly `limit` bytes, peek to check for
        // overflow on the *current* call rather than waiting for the next one.
        if self.consumed == self.limit && n > 0 {
            let mut probe = [0u8; 1];
            match self.inner.read(&mut probe) {
                Ok(0) => {
                    self.at_eof = true;
                }
                Ok(_) => {
                    return Err(LimitExceeded { limit: self.limit }.into_io_error());
                }
                Err(e) => return Err(e),
            }
        }

        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// LimitedWriter
// ---------------------------------------------------------------------------

/// An [`AsyncWrite`] adaptor that returns a hard error when the total bytes
/// written exceeds `limit`.
///
/// Used to cap blob downloads mid-stream (e.g. `pull_blob` from
/// `oci-distribution`) so a lying or malicious manifest cannot force
/// unbounded memory growth.
///
/// When the limit is hit the error message includes the limit value so
/// callers can surface a human-readable diagnostic.
pub struct LimitedWriter<W> {
    inner: W,
    limit: u64,
    written: u64,
}

impl<W: AsyncWrite + Unpin> LimitedWriter<W> {
    /// Wrap `inner`, refusing to accept more than `limit` bytes in total.
    pub const fn new(inner: W, limit: u64) -> Self {
        Self {
            inner,
            limit,
            written: 0,
        }
    }

    /// Consume the wrapper and return the inner writer.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for LimitedWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let remaining = self.limit.saturating_sub(self.written);
        if remaining == 0 {
            return Poll::Ready(Err(io::Error::other(format!(
                "blob download exceeds limit of {} bytes",
                self.limit
            ))));
        }

        // Clamp the write to at most `remaining` bytes.
        let capped_len = buf
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));

        let result = Pin::new(&mut self.inner).poll_write(cx, &buf[..capped_len]);

        if let Poll::Ready(Ok(n)) = result {
            self.written = self
                .written
                .saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
            // If we just hit the exact limit, check whether the caller is
            // sending more data (i.e. the next byte would overflow).  We
            // cannot peek asynchronously here, so we simply update written
            // and let the *next* poll_write call return the error.
        }

        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

// LimitedWriter is Unpin because its only non-marker field is W: Unpin, and
// all others (u64) are always Unpin.
impl<W: Unpin> Unpin for LimitedWriter<W> {}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use tokio::io::AsyncWriteExt as _;

    use super::*;

    // ── would_exceed_cap tests ──────────────────────────────────────────────

    #[test]
    fn under_cap_does_not_trip() {
        assert!(!would_exceed_cap(10, 20, 100));
    }

    #[test]
    fn exactly_at_cap_does_not_trip() {
        // Reaching the cap exactly is allowed; only *exceeding* it fails.
        assert!(!would_exceed_cap(60, 40, 100));
    }

    #[test]
    fn one_byte_over_cap_trips() {
        assert!(would_exceed_cap(60, 41, 100));
    }

    #[test]
    fn single_oversized_chunk_trips_before_extend() {
        // Regression: the buffer is empty but a single huge chunk would blow
        // past the cap.  The guard must fail *before* the chunk is appended,
        // so the oversized allocation never happens.
        assert!(would_exceed_cap(0, 10_000, 1024));
    }

    #[test]
    fn saturating_arithmetic_does_not_overflow() {
        // Pathological lengths must saturate, not wrap, and still trip the cap.
        assert!(would_exceed_cap(
            usize::MAX,
            usize::MAX,
            MAX_BLOB_COMPRESSED_BYTES
        ));
    }

    // ── LimitedReader tests ─────────────────────────────────────────────────

    #[test]
    fn under_limit_reads_all() {
        let data = b"hello world";
        let mut reader = LimitedReader::new(&data[..], 64);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("read_to_end");
        assert_eq!(out, data);
    }

    #[test]
    fn exactly_at_limit_succeeds() {
        let data = b"12345";
        let mut reader = LimitedReader::new(&data[..], 5);
        let mut out = Vec::new();
        reader
            .read_to_end(&mut out)
            .expect("should succeed at limit");
        assert_eq!(out, data);
    }

    #[test]
    fn over_limit_errors_not_truncates() {
        let data = b"hello world"; // 11 bytes
        let mut reader = LimitedReader::new(&data[..], 5);
        let mut out = Vec::new();
        let result = reader.read_to_end(&mut out);
        assert!(
            result.is_err(),
            "expected error, got Ok with {} bytes",
            out.len()
        );
        // Must be an error — NOT a silently truncated Ok.
    }

    #[test]
    fn limit_zero_errors_immediately() {
        let data = b"x";
        let mut reader = LimitedReader::new(&data[..], 0);
        let mut buf = [0u8; 1];
        let result = reader.read(&mut buf);
        assert!(result.is_err(), "limit=0 should error on first read");
    }

    // ── LimitedWriter tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn writer_exactly_at_cap_succeeds() {
        let inner: Vec<u8> = Vec::new();
        let mut writer = LimitedWriter::new(inner, 5);
        writer
            .write_all(b"12345")
            .await
            .expect("should succeed at cap");
        let out = writer.into_inner();
        assert_eq!(out, b"12345");
    }

    #[tokio::test]
    async fn writer_over_cap_errors() {
        let inner: Vec<u8> = Vec::new();
        let mut writer = LimitedWriter::new(inner, 5);
        // First 5 bytes should be accepted.
        writer.write_all(b"12345").await.expect("first 5 bytes ok");
        // One more byte must error.
        let result = writer.write_all(b"6").await;
        assert!(result.is_err(), "writing past cap should return an error");
    }

    #[tokio::test]
    async fn writer_under_cap_succeeds() {
        let inner: Vec<u8> = Vec::new();
        let mut writer = LimitedWriter::new(inner, 100);
        writer.write_all(b"hello").await.expect("well under cap");
        let out = writer.into_inner();
        assert_eq!(out, b"hello");
    }
}
