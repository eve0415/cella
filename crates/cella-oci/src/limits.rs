//! Blob size caps to defend against zip-bombs and oversized blobs.
//!
//! [`LimitedReader`] wraps any [`Read`] and returns a hard [`io::Error`] when
//! the byte count exceeds the configured limit — it never silently truncates.

use std::io::{self, Read};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum compressed (downloaded) blob size: 512 MiB.
pub const MAX_BLOB_COMPRESSED_BYTES: u64 = 512 * 1024 * 1024;

/// Maximum decompressed output size: 2 GiB.
pub const MAX_BLOB_DECOMPRESSED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

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
                Ok(_) => Err(io::Error::other(format!(
                    "decompressed output exceeds limit of {} bytes",
                    self.limit
                ))),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
                Err(e) => Err(e),
            };
        }

        // Only ask the inner reader for `remaining` bytes at most.
        let capped_len = buf
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..capped_len])?;
        self.consumed += n as u64;

        // If we've now consumed exactly `limit` bytes, peek to check for
        // overflow on the *current* call rather than waiting for the next one.
        if self.consumed == self.limit && n > 0 {
            let mut probe = [0u8; 1];
            match self.inner.read(&mut probe) {
                Ok(0) => {
                    self.at_eof = true;
                }
                Ok(_) => {
                    return Err(io::Error::other(format!(
                        "decompressed output exceeds limit of {} bytes",
                        self.limit
                    )));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }

        Ok(n)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;

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
}
