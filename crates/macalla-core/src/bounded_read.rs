// SPDX-License-Identifier: MIT
//! A byte cap that holds after decompression, not just on the wire.
//!
//! `ureq` 3.4.1's `BodyWithConfig::limit()` sits beneath its gzip/brotli
//! decoder: `src/body/mod.rs` builds `BodyReader` as
//! `MaybeLossyDecoder<CharsetDecoder<ContentDecoder<LimitReader<...>>>>`, so
//! `LimitReader` bounds the raw wire bytes, and `ContentDecoder`
//! (decompression) sits *outside* it, reading from it as much as it needs to
//! produce however much decoded output is asked for. A small gzipped reply
//! can therefore inflate into an arbitrarily large `String` even with
//! `.limit()` set. [`read_capped`] is the one place `macalla-engines` and
//! `macalla-enhancers` each bound a listing reply on the decoded byte count
//! instead, without pulling in a dependency neither crate already has --
//! `std::io::Read` and its `Take` adapter are all this needs.

use std::io::{self, Read};

/// Read `reader` to a `String`, failing if more than `limit` bytes come out
/// of it. Checked on `reader`'s own output, so this is safe to put after a
/// decompressing reader (gzip, brotli) where an upstream compressed-byte
/// limit is not: put `read_capped` around the fully-decoded reader (for
/// example `ureq`'s `Body::with_config().reader()`, which decodes before
/// this ever sees a byte), not around the raw socket.
///
/// Reads at most `limit + 1` bytes from `reader` -- never more, whatever
/// `reader` is willing to produce -- then fails if that one extra byte
/// arrived, rather than reading out however large the reply actually is
/// first.
pub fn read_capped(mut reader: impl Read, limit: u64) -> io::Result<String> {
    let mut buf = Vec::new();
    reader.by_ref().take(limit + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the reply was more than {limit} bytes"),
        ));
    }
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_the_limit_succeeds() {
        let body = "a".repeat(10);
        assert_eq!(read_capped(body.as_bytes(), 10).unwrap(), body);
    }

    #[test]
    fn one_byte_over_the_limit_fails() {
        let body = "a".repeat(11);
        let error = read_capped(body.as_bytes(), 10).unwrap_err();
        assert!(error.to_string().contains("more than 10 bytes"), "{error}");
    }

    #[test]
    fn an_endless_reader_is_never_read_unbounded() {
        // std::io::repeat never runs out on its own; if read_capped read
        // past `limit + 1` bytes this test would hang instead of finishing.
        let error = read_capped(io::repeat(b'x'), 10).unwrap_err();
        assert!(error.to_string().contains("more than 10 bytes"), "{error}");
    }

    #[test]
    fn invalid_utf8_within_the_limit_still_fails() {
        let bytes: &[u8] = &[0xff, 0xfe, 0xfd];
        assert!(read_capped(bytes, 10).is_err());
    }
}
