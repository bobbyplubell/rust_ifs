//! Tile-histogram transport: encode/decode an `Accum` to the SAME wire format
//! `coordinator/src/histio.rs` uses, so a piece a native worker uploads is
//! byte-compatible with the existing coordinator/web decode path.
//!
//! Format (matches `coordinator/src/histio.rs` + `web/js/contribute.js`):
//!   `hist_b64 = base64( zlib-deflate( LE-u64 of the flat w*h*4 histogram cells ) )`
//!
//! The flat cell vector is `accum.data` flattened in `[r, g, b, count]` per-cell,
//! row-major order — exactly `accum.data.iter().flatten()` (the same layout
//! `coordinator/src/render.rs::content_hash` packs back via `chunks_exact(4)`).
//! The coordinator magic-sniffs zstd vs zlib/deflate on decode; we always emit
//! zlib (matching `histio::encode_hist`, which the browser's `CompressionStream`
//! ('deflate') also produces).

use std::io::Read;

use base64::Engine as _;
use flame_core::render::Accum;

/// Encode an `Accum` to `base64(zlib-deflate(LE u64 cells))` — the engine-side
/// inverse of [`decode_accum`], byte-identical to `histio::encode_hist`.
pub fn encode_accum(accum: &Accum) -> String {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    // Flat cells: [r, g, b, count] per cell, row-major — `accum.data` flattened.
    let mut raw = Vec::with_capacity(accum.data.len() * 4 * 8);
    for cell in &accum.data {
        for v in cell {
            raw.extend_from_slice(&v.to_le_bytes());
        }
    }
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&raw).expect("zlib write to Vec cannot fail");
    let compressed = enc.finish().expect("zlib finish to Vec cannot fail");
    base64::engine::general_purpose::STANDARD.encode(compressed)
}

/// Decode a `hist_b64` payload back into an `Accum` of dimensions `w x h`.
///
/// Accepts zstd or zlib/deflate (magic-sniffed, mirroring `histio::decode_hist`)
/// so it round-trips both our own `encode_accum` and the coordinator/browser
/// encoders. Returns `None` on bad base64, a decompression failure, or a
/// decompressed length that doesn't match `w*h*4*8` bytes.
pub fn decode_accum(hist_b64: &str, w: usize, h: usize) -> Option<Accum> {
    let compressed = base64::engine::general_purpose::STANDARD
        .decode(hist_b64.trim())
        .ok()?;
    let want_bytes = w * h * 4 * 8;
    let raw = decompress(&compressed, want_bytes)?;
    if raw.len() != want_bytes {
        return None;
    }
    let mut accum = Accum::new(w, h);
    for (cell, chunk) in accum.data.iter_mut().zip(raw.chunks_exact(32)) {
        for (i, q) in chunk.chunks_exact(8).enumerate() {
            cell[i] = u64::from_le_bytes(q.try_into().unwrap());
        }
    }
    Some(accum)
}

/// Try zstd (magic `28 B5 2F FD`), then zlib, then raw deflate. Bounded by
/// `cap` so a hostile payload can't blow up memory (mirrors `histio`).
fn decompress(compressed: &[u8], cap: usize) -> Option<Vec<u8>> {
    let limit = (cap + 1) as u64;

    if compressed.len() >= 4 && compressed[0..4] == [0x28, 0xB5, 0x2F, 0xFD] {
        let mut out = Vec::new();
        let dec = zstd::stream::read::Decoder::new(compressed).ok()?;
        dec.take(limit).read_to_end(&mut out).ok()?;
        return Some(out);
    }

    let mut out = Vec::new();
    let dec = flate2::read::ZlibDecoder::new(compressed);
    if dec.take(limit).read_to_end(&mut out).is_ok() && !out.is_empty() {
        return Some(out);
    }
    out.clear();
    let dec = flate2::read::DeflateDecoder::new(compressed);
    dec.take(limit).read_to_end(&mut out).ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flame_core::chunked::{hist_hash_hex, render_batch};
    use flame_core::genome::Genome;
    use flame_core::rng::Rng;

    /// A small accum to keep the test fast (8x8, few samples). Channel ordering
    /// is the protocol's `[r, g, b, count]` per cell.
    fn small_accum() -> Accum {
        let mut rng = Rng::new(2);
        let genome = Genome::random(&mut rng, 3);
        let id = [9u8; 32];
        // 8x8, ss=1, 2_000 spp — enough to populate cells, fast to encode.
        render_batch(&genome, &id, 0, 0, 8, 8, 1, 2_000, 128)
    }

    /// encode_accum → decode_accum reproduces the SAME Accum bit-for-bit, so the
    /// content hash (hist_hash) is identical. This is the property the protocol
    /// relies on: hist_b64 carries the exact histogram the hash commits to.
    #[test]
    fn encode_decode_round_trips_to_identical_accum_and_hash() {
        let accum = small_accum();
        let b64 = encode_accum(&accum);
        let decoded = decode_accum(&b64, 8, 8).expect("decode our own payload");
        assert_eq!(decoded.w, accum.w);
        assert_eq!(decoded.h, accum.h);
        assert_eq!(decoded.data, accum.data, "round-trip must be byte-identical");
        assert_eq!(
            hist_hash_hex(&decoded),
            hist_hash_hex(&accum),
            "content hash survives the hist_b64 round-trip"
        );
    }

    /// The wire bytes match `coordinator/src/histio.rs` exactly: base64 of a
    /// zlib stream (magic byte 0x78) over the flat LE-u64 `[r,g,b,count]` cells.
    /// This is the same pipeline `histio::decode_hist` (zlib branch) reads and
    /// the browser's `CompressionStream('deflate')` emits, so an accumulator /
    /// coordinator decodes a native worker's piece unchanged.
    #[test]
    fn wire_format_is_base64_zlib_le_u64_matching_histio() {
        let accum = small_accum();
        let b64 = encode_accum(&accum);
        let compressed = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .unwrap();
        // zlib header magic (what histio's zlib branch + the browser expect).
        assert_eq!(compressed[0], 0x78, "must be a zlib stream");

        // Independently inflate + reinterpret as flat cells exactly like
        // histio::decode_hist + coordinator's content_hash packing.
        let raw = decompress(&compressed, 8 * 8 * 4 * 8).unwrap();
        assert_eq!(raw.len(), 8 * 8 * 4 * 8);
        let cells: Vec<u64> = raw
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        // Pack back into an Accum via chunks_exact(4) — the coordinator's path.
        let mut rebuilt = Accum::new(8, 8);
        for (cell, src) in rebuilt.data.iter_mut().zip(cells.chunks_exact(4)) {
            cell.copy_from_slice(src);
        }
        assert_eq!(rebuilt.data, accum.data, "flat cell layout matches histio");
    }

    /// A short/garbage payload decodes to None, not a panic (untrusted input).
    #[test]
    fn bad_payload_rejected() {
        assert!(decode_accum("!!!not base64!!!", 8, 8).is_none());
        let too_small = encode_accum(&Accum::new(4, 4));
        assert!(decode_accum(&too_small, 8, 8).is_none(), "wrong dims rejected");
    }
}
