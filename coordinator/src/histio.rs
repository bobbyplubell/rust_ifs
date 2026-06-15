//! Histogram transport: decode the base64 + (deflate|zstd) `hist` payload from
//! a Result into a flat `Vec<u64>` of integer cells.
//!
//! The client encodes the tile histogram's little-endian u64 bytes; we sniff
//! the compression by trying zstd first, then zlib/deflate. Both are bounded
//! by `spec::max_hist_bytes()` so a malicious payload can't blow up memory.

use base64::Engine;
use std::io::Read;

use crate::error::{ApiError, ApiResult};
use crate::spec;

/// Decode `hist` (base64 of compressed LE-u64 bytes) into the flat cell vector.
/// Validates the decompressed length matches the spec histogram size exactly.
pub fn decode_hist(hist_b64: &str) -> ApiResult<Vec<u64>> {
    let compressed = base64::engine::general_purpose::STANDARD
        .decode(hist_b64.trim())
        .map_err(|e| ApiError::bad(format!("hist not valid base64: {e}")))?;

    // Bound the input too: a tile compresses to well under the raw size, but a
    // hostile client could send junk. Cap pre-decode at the raw cap.
    if compressed.len() > spec::max_hist_bytes() {
        return Err(ApiError::bad("hist payload too large"));
    }

    let raw = decompress(&compressed)?;
    let want = spec::max_hist_bytes();
    if raw.len() != want {
        return Err(ApiError::bad(format!(
            "hist wrong size: got {} bytes, expected {want}",
            raw.len()
        )));
    }

    let mut cells = Vec::with_capacity(spec::hist_cells());
    for chunk in raw.chunks_exact(8) {
        cells.push(u64::from_le_bytes(chunk.try_into().unwrap()));
    }
    Ok(cells)
}

/// Encode a flat cell vector to base64(deflate(LE u64 bytes)) — the inverse of
/// `decode_hist`, used by tests and the smoke client.
#[allow(dead_code)]
pub fn encode_hist(cells: &[u64]) -> String {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    let mut raw = Vec::with_capacity(cells.len() * 8);
    for c in cells {
        raw.extend_from_slice(&c.to_le_bytes());
    }
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&raw).unwrap();
    let compressed = enc.finish().unwrap();
    base64::engine::general_purpose::STANDARD.encode(compressed)
}

/// Try zstd, then zlib/deflate. Decompression is bounded by `take()` so a
/// decompression bomb can't exceed the spec cap by more than one read.
fn decompress(compressed: &[u8]) -> ApiResult<Vec<u8>> {
    let cap = spec::max_hist_bytes();

    // zstd: a frame begins with magic 0x28 0xB5 0x2F 0xFD.
    if compressed.len() >= 4 && compressed[0..4] == [0x28, 0xB5, 0x2F, 0xFD] {
        let mut out = Vec::new();
        let dec = zstd::stream::read::Decoder::new(compressed)
            .map_err(|e| ApiError::bad(format!("zstd init: {e}")))?;
        dec.take((cap + 1) as u64)
            .read_to_end(&mut out)
            .map_err(|e| ApiError::bad(format!("zstd decode: {e}")))?;
        return Ok(out);
    }

    // zlib / deflate.
    let mut out = Vec::new();
    let dec = flate2::read::ZlibDecoder::new(compressed);
    if dec.take((cap + 1) as u64).read_to_end(&mut out).is_ok() && !out.is_empty() {
        return Ok(out);
    }
    // Fall back to raw deflate (no zlib header).
    out.clear();
    let dec = flate2::read::DeflateDecoder::new(compressed);
    dec.take((cap + 1) as u64)
        .read_to_end(&mut out)
        .map_err(|e| ApiError::bad(format!("deflate decode: {e}")))?;
    Ok(out)
}
