//! Decoding of `Content-Encoding` response bodies.
//!
//! The native transport gets this from reqwest, which decodes gzip and brotli
//! transparently. The WASI transport sees the raw bytes from `wasi:http` and
//! decodes them here. A body whose coding this module cannot decode is refused
//! by the caller; it is never returned as undecoded bytes.

use std::io::{self, Read};

use flate2::read::{GzDecoder, ZlibDecoder};

/// `Accept-Encoding` for requests that set none: the codings [`read_capped`]
/// decodes that are worth asking for. Deflate is still decoded when a server
/// sends it anyway, as the native transport does.
pub const ACCEPT_ENCODING: &str = "gzip, br";

/// Input bytes the brotli decoder pulls per refill.
const BROTLI_BUFFER: usize = 4096;

/// A content coding this module decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coding {
    Identity,
    Gzip,
    Deflate,
    Brotli,
}

/// The coding of a body from its `Content-Encoding` header values: several
/// headers, or one header holding a comma-separated list. `identity` and empty
/// tokens are ignored. Err carries the codings this module cannot decode,
/// including a stack of two or more codings.
pub fn coding_of<'a>(values: impl IntoIterator<Item = &'a str>) -> Result<Coding, String> {
    let mut tokens = Vec::new();
    for value in values {
        for token in value.split(',').map(str::trim) {
            if token.is_empty() || token.eq_ignore_ascii_case("identity") {
                continue;
            }
            tokens.push(token.to_ascii_lowercase());
        }
    }
    match tokens.as_slice() {
        [] => Ok(Coding::Identity),
        [only] => match only.as_str() {
            "gzip" | "x-gzip" => Ok(Coding::Gzip),
            "deflate" => Ok(Coding::Deflate),
            "br" => Ok(Coding::Brotli),
            _ => Err(only.clone()),
        },
        _ => Err(tokens.join(", ")),
    }
}

/// Counts the bytes a decoder pulls from its source, so that an empty body is
/// not mistaken for a truncated one.
struct Counted<R> {
    inner: R,
    count: usize,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count += n;
        Ok(n)
    }
}

/// Read `src` to its end, decoded under `coding`, keeping at most `cap` decoded
/// bytes. Past the cap nothing more is read, so a decompression bomb costs
/// `cap` bytes of output. A body with no bytes at all decodes to nothing.
pub fn read_capped<R: Read>(src: R, coding: Coding, cap: usize) -> io::Result<Vec<u8>> {
    let mut counted = Counted {
        inner: src,
        count: 0,
    };
    let mut out = Vec::new();
    let limit = cap as u64;
    let result = match coding {
        Coding::Identity => (&mut counted).take(limit).read_to_end(&mut out),
        Coding::Gzip => GzDecoder::new(&mut counted)
            .take(limit)
            .read_to_end(&mut out),
        Coding::Deflate => ZlibDecoder::new(&mut counted)
            .take(limit)
            .read_to_end(&mut out),
        Coding::Brotli => brotli_decompressor::Decompressor::new(&mut counted, BROTLI_BUFFER)
            .take(limit)
            .read_to_end(&mut out),
    };
    match result {
        Ok(_) => Ok(out),
        Err(e)
            if counted.count == 0
                && matches!(
                    e.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
                ) =>
        {
            Ok(Vec::new())
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    fn brotli(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut out = Vec::new();
        {
            let mut enc = brotli::CompressorWriter::new(&mut out, 4096, 5, 22);
            enc.write_all(data).unwrap();
        }
        out
    }

    #[test]
    fn gzip_body_is_decoded() {
        let body = br#"{"gzipped":true,"method":"GET"}"#;
        let out = read_capped(&gzip(body)[..], Coding::Gzip, 1024).unwrap();
        assert_eq!(out, body);
    }

    #[test]
    fn deflate_and_brotli_bodies_are_decoded() {
        let body = b"hello, compressed world";
        assert_eq!(
            read_capped(&zlib(body)[..], Coding::Deflate, 1024).unwrap(),
            body
        );
        assert_eq!(
            read_capped(&brotli(body)[..], Coding::Brotli, 1024).unwrap(),
            body
        );
    }

    #[test]
    fn identity_body_is_copied() {
        let out = read_capped(&b"plain"[..], Coding::Identity, 1024).unwrap();
        assert_eq!(out, b"plain");
    }

    #[test]
    fn decoded_size_is_capped_not_the_compressed_size() {
        // 4 MiB of zeros compresses to a few KiB; the cap bounds the output.
        let big = vec![0u8; 4 * 1024 * 1024];
        let packed = gzip(&big);
        assert!(packed.len() < 64 * 1024, "test input should compress well");
        let out = read_capped(&packed[..], Coding::Gzip, 1024 * 1024).unwrap();
        assert_eq!(out.len(), 1024 * 1024);
    }

    #[test]
    fn small_body_under_the_cap_is_whole() {
        let out = read_capped(&gzip(b"abc")[..], Coding::Gzip, 3).unwrap();
        assert_eq!(out, b"abc");
    }

    #[test]
    fn empty_encoded_body_is_empty_not_an_error() {
        let empty: &[u8] = &[];
        assert_eq!(read_capped(empty, Coding::Gzip, 64).unwrap(), b"");
        assert_eq!(read_capped(empty, Coding::Brotli, 64).unwrap(), b"");
    }

    #[test]
    fn corrupt_gzip_is_an_error_not_garbage() {
        let err = read_capped(&b"\x1f\x8bnot really gzip"[..], Coding::Gzip, 64).unwrap_err();
        assert_ne!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn truncated_gzip_is_an_error() {
        let mut packed = gzip(&vec![b'x'; 10_000]);
        packed.truncate(packed.len() / 2);
        assert!(read_capped(&packed[..], Coding::Gzip, 1 << 20).is_err());
    }

    #[test]
    fn coding_is_read_from_the_header() {
        assert_eq!(coding_of(std::iter::empty()), Ok(Coding::Identity));
        assert_eq!(coding_of(["identity"]), Ok(Coding::Identity));
        assert_eq!(coding_of([""]), Ok(Coding::Identity));
        assert_eq!(coding_of(["gzip"]), Ok(Coding::Gzip));
        assert_eq!(coding_of([" GZIP "]), Ok(Coding::Gzip));
        assert_eq!(coding_of(["x-gzip"]), Ok(Coding::Gzip));
        assert_eq!(coding_of(["deflate"]), Ok(Coding::Deflate));
        assert_eq!(coding_of(["br"]), Ok(Coding::Brotli));
        assert_eq!(coding_of(["identity, gzip"]), Ok(Coding::Gzip));
        assert_eq!(coding_of(["gzip", "identity"]), Ok(Coding::Gzip));
    }

    #[test]
    fn unsupported_coding_is_refused() {
        assert_eq!(coding_of(["zstd"]), Err("zstd".to_string()));
        assert_eq!(coding_of(["compress"]), Err("compress".to_string()));
    }

    #[test]
    fn stacked_codings_are_refused() {
        assert_eq!(coding_of(["gzip, br"]), Err("gzip, br".to_string()));
        assert_eq!(coding_of(["gzip", "gzip"]), Err("gzip, gzip".to_string()));
    }
}
