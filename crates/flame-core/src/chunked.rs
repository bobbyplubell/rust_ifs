//! Chunked (progressive / provable) rendering.
//!
//! A protocol render is the element-wise sum of `n_chunks` *independent* chunk
//! accumulations. Chunk `i` runs its own burn-in and `samples_per_chunk`
//! plotted iterations from `seed = chunk_seed(challenge, i)`. Because the
//! histogram is additive, the running sum can be tone-mapped at any point
//! (progressive display), and each chunk's own buffer can be hashed as a
//! verifiable unit of work (render proofs / audits).
//!
//! Hashing is SHA-256 everywhere; hex is lowercase. These byte layouts are
//! protocol constants — changing any of them is a protocol break (the golden
//! test below is the alarm, not an inconvenience to update silently).

use sha2::{Digest, Sha256};

use crate::genome::Genome;
use crate::render::{accumulate, Accum};

/// Burn-in iterations per chunk (each chunk settles onto the attractor
/// independently).
pub const CHUNK_BURN_IN: u64 = 20;

/// A challenge seed: 32 bytes (passed as lowercase hex in string APIs).
pub type Challenge = [u8; 32];

/// Convenience challenge for casual (non-proof) renders:
/// `sha256(le64(seed))`.
pub fn challenge_from_seed(seed: u64) -> Challenge {
    let digest = Sha256::digest(seed.to_le_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Per-chunk PRNG seed:
/// `u64::from_le_bytes(sha256(challenge ‖ le32(idx))[0..8])`.
pub fn chunk_seed(challenge: &Challenge, idx: u32) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(challenge);
    hasher.update(idx.to_le_bytes());
    let digest = hasher.finalize();
    let mut first8 = [0u8; 8];
    first8.copy_from_slice(&digest[0..8]);
    u64::from_le_bytes(first8)
}

/// Render chunk `idx` into its own fresh accumulation buffer at supersampled
/// resolution (`width*ss x height*ss`).
pub fn render_chunk(
    genome: &Genome,
    width: usize,
    height: usize,
    ss: usize,
    samples_per_chunk: u64,
    challenge: &Challenge,
    idx: u32,
) -> Accum {
    let mut accum = Accum::new(width * ss, height * ss);
    let seed = chunk_seed(challenge, idx);
    accumulate(genome, samples_per_chunk, CHUNK_BURN_IN, seed, &mut accum);
    accum
}

/// Protocol v3 "loop proof" frame: the proof's unit of work is one frame of
/// the sheep's animation loop instead of one chunk of a still. Frame `idx` of
/// `n_frames` is the genome animated to phase `idx / n_frames`, rendered as
/// `temporal` sub-steps spanning one frame interval (flam3 temporal samples,
/// so the proven loop plays back with motion blur):
///
///   accum(idx) = Σ_{k=0..T-1} accumulate(
///       animated(genome, idx/N + k/(N*T)),
///       samples_per_frame / T, CHUNK_BURN_IN,
///       seed = chunk_seed(challenge, idx) ^ k)
///
/// Deterministic: `animated` uses fmath only, the phase arithmetic is plain
/// IEEE division of small integers. Hash with `chunk_hash` as usual. Auditing
/// one frame costs 1/N of the full proof, same asymmetry as chunk audits.
pub fn render_proof_frame(
    genome: &Genome,
    width: usize,
    height: usize,
    ss: usize,
    samples_per_frame: u64,
    challenge: &Challenge,
    idx: u32,
    n_frames: u32,
    temporal: u32,
) -> Accum {
    let mut accum = Accum::new(width * ss, height * ss);
    let t = temporal.max(1) as u64;
    let per_step = (samples_per_frame / t).max(1);
    let seed = chunk_seed(challenge, idx);
    for k in 0..t {
        let phase = idx as f64 / n_frames as f64 + k as f64 / (n_frames as f64 * t as f64);
        let g = crate::animate::animated(genome, phase);
        accumulate(&g, per_step, CHUNK_BURN_IN, seed ^ k, &mut accum);
    }
    accum
}

#[cfg(test)]
mod proof_frame_tests {
    use super::*;
    use crate::rng::Rng;

    #[test]
    fn proof_frames_are_deterministic_and_distinct() {
        let mut rng = Rng::new(7);
        let genome = Genome::random(&mut rng, 3);
        let challenge = challenge_from_seed(7);
        let a = chunk_hash_hex(&render_proof_frame(&genome, 64, 64, 1, 30_000, &challenge, 3, 64, 2));
        let b = chunk_hash_hex(&render_proof_frame(&genome, 64, 64, 1, 30_000, &challenge, 3, 64, 2));
        let c = chunk_hash_hex(&render_proof_frame(&genome, 64, 64, 1, 30_000, &challenge, 4, 64, 2));
        assert_eq!(a, b, "same frame must hash identically");
        assert_ne!(a, c, "different frames must differ");
    }
}

/// Chunk hash: SHA-256 over the chunk's own accumulation buffer (NOT the
/// running sum) — cells row-major, each cell serialized as 4 x f64
/// little-endian in order `[r_sum, g_sum, b_sum, count]` (32 bytes/cell).
pub fn chunk_hash(accum: &Accum) -> [u8; 32] {
    let mut hasher = Sha256::new();
    let mut row = Vec::with_capacity(accum.w * 32);
    for y in 0..accum.h {
        row.clear();
        for cell in &accum.data[y * accum.w..(y + 1) * accum.w] {
            for v in cell {
                row.extend_from_slice(&v.to_le_bytes());
            }
        }
        hasher.update(&row);
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// `chunk_hash` as lowercase hex.
pub fn chunk_hash_hex(accum: &Accum) -> String {
    to_hex(&chunk_hash(accum))
}

/// SHA-256 of arbitrary bytes as lowercase hex (e.g. the final RGBA image).
pub fn sha256_hex(bytes: &[u8]) -> String {
    to_hex(&Sha256::digest(bytes))
}

/// Lowercase hex encoding.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Parse a 64-char hex string into a 32-byte challenge.
pub fn challenge_from_hex(hex: &str) -> Result<Challenge, String> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return Err(format!("challenge must be 64 hex chars, got {}", hex.len()));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|e| format!("bad hex at byte {i}: {e}"))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::tonemap;
    use crate::rng::Rng;

    /// Golden determinism corpus (PROTOCOL CONSTANTS).
    ///
    /// Genome::random from seeds 2, 3, 7 (3 transforms); 64x64, ss 1,
    /// 8 chunks x 50_000 samples, challenge = challenge_from_seed(seed).
    ///
    /// The expected hashes were generated by running this exact code once and
    /// committing the output. If this test fails, the renderer's bitstream has
    /// changed — that is a PROTOCOL BREAK (every render proof in the wild
    /// becomes unverifiable), not a test to silently update. Only regenerate
    /// these constants as part of a deliberate, versioned protocol change.
    const GOLDEN: [(u64, [&str; 8], &str); 3] = [
        (
            2,
            [
                "85f402acc68b47624fbea59da2a8a7ed6fa28f5b920ce5b2181a5a5c5ef22e65",
                "e430eaf0294da178b8e6c0a862370ff3d33d54fbeedf513e49757fad1b445526",
                "f719eb9668761ba0ff3161b94fffa0c947499145eae4f32440ae1f2e9a257580",
                "f746c22c5c7559b135bc38cd9af0b1b4a985e58a26a79c82e761ba48fabd13ba",
                "0ce62342dc321c73a748c0aea9bef7e0a8e97b717ae340adaf262d4627fb9934",
                "f9c1c59d4fc26ff43595af970170569f1c248aa200eb9a9e34584c4f619be5b3",
                "9709ddb742d31f055352782089f807552be1d34885779f74e945c19508644f42",
                "913e2c037ff7cde87e666a8439dc86476977215ca36284197267e6939b289714",
            ],
            "fbe401d9ec7d1e587bfb9207b8fa9a0cccaf2c4ff02a8cfb56a1727996d64d21",
        ),
        (
            3,
            [
                "6353992bed8724b112e9b1e9bcded367de77382e502722ee6cdf62093b69edc2",
                "3eea588f15bc4d3976c21526dda7b63a2d71e09e6299398833c1725d1d32f3b6",
                "bd827a69fc1951226ff27bfaa635dae07c25dd5ccf6c9ecf97b2922fd7b06aa1",
                "fc0430ce3ec1b7424808525cdbc952691f2ee475772022c354fbe6cf58b83b64",
                "2f8ef562b09f295fe9316fa481682ecdd5e6a92c7b47e619c070bd1a249a02d7",
                "a3c70bbbe5e23e1587157079d02e9a5559c7bfc1448e38f4f5c947ba82965a41",
                "8c3ce9e5b8472651925eb3a7fbdaef588fd617f98a18064a2efc6a2338b48b08",
                "061fa7bc6479da06702da4eb3dd1d138c7ca78fb935f3a53cdfa329eab8de18f",
            ],
            "6026a3eec08693d2123fe35c18dd23f7add7f1a8da3b36cc0e2858735622ca02",
        ),
        (
            7,
            [
                "4259569fd1e7cc98846f5cb756ee61509ec2826b9b1a494ad1a0a20dd14d3ae9",
                "47fea99c8e66945c18c69b34c52e7f5f86a14250a336714acf811a520f74095a",
                "6f2d0312b60bb299bd3725e1f29ccf518ad9752a82fdddc181c109a2b5c6a849",
                "1534ae67cbdbeeeb4eaeb91dafe66a2113faef4957f6cbdd590211adfd17fd7c",
                "42b565d43e82066dca1b218997319bf25b5ecd96f60da6ca77ca2493fead860e",
                "66acf2e159bd083e9c52116b7b9fffdff4d434e099a592f89231a6fbafda15f3",
                "bb4d7888eadd81c25472c6794995e2d570edd79fce03e5edb8f86fb3d20fe777",
                "5bd644a1038841323645a13da649a930d6dfdb17b3639bca694ac56269555d99",
            ],
            "72009198c73c4f0125eb80f4fe7f0d37eee243acca24b76f0b92a381f0bbcdf0",
        ),
    ];

    const W: usize = 64;
    const H: usize = 64;
    const SS: usize = 1;
    const N_CHUNKS: u32 = 8;
    const SAMPLES_PER_CHUNK: u64 = 50_000;

    fn run_corpus_entry(seed: u64) -> (Vec<String>, String) {
        let mut rng = Rng::new(seed);
        let genome = Genome::random(&mut rng, 3);
        let challenge = challenge_from_seed(seed);

        let mut running = Accum::new(W * SS, H * SS);
        let mut hashes = Vec::new();
        for idx in 0..N_CHUNKS {
            let chunk = render_chunk(&genome, W, H, SS, SAMPLES_PER_CHUNK, &challenge, idx);
            hashes.push(chunk_hash_hex(&chunk));
            running.merge(&chunk);
        }
        let rgba = tonemap(&running, &genome, W, H, SS);
        (hashes, sha256_hex(&rgba))
    }

    #[test]
    #[ignore = "generator for the golden constants; run manually with --ignored --nocapture"]
    fn print_golden() {
        for seed in [2u64, 3, 7] {
            let (hashes, rgba_hash) = run_corpus_entry(seed);
            for (idx, h) in hashes.iter().enumerate() {
                println!("seed {seed} chunk {idx}: \"{h}\",");
            }
            println!("seed {seed} rgba: \"{rgba_hash}\",");
        }
    }

    #[test]
    fn golden_determinism() {
        for (seed, expected_chunks, expected_rgba) in GOLDEN {
            let (hashes, rgba_hash) = run_corpus_entry(seed);
            for (idx, (got, want)) in hashes.iter().zip(expected_chunks.iter()).enumerate() {
                assert_eq!(
                    got, want,
                    "PROTOCOL BREAK: chunk {idx} hash changed for seed {seed}"
                );
            }
            assert_eq!(
                rgba_hash, expected_rgba,
                "PROTOCOL BREAK: final RGBA hash changed for seed {seed}"
            );
        }
    }

    /// Merging chunks must equal accumulating them in any grouping
    /// (histogram additivity — what makes progressive display and proofs the
    /// same computation).
    #[test]
    fn chunk_merge_is_elementwise_add() {
        let mut rng = Rng::new(2);
        let genome = Genome::random(&mut rng, 3);
        let challenge = challenge_from_seed(2);

        let a = render_chunk(&genome, 16, 16, 2, 5_000, &challenge, 0);
        let b = render_chunk(&genome, 16, 16, 2, 5_000, &challenge, 1);
        let mut ab = Accum::new(32, 32);
        ab.merge(&a);
        ab.merge(&b);
        let mut ba = Accum::new(32, 32);
        ba.merge(&b);
        ba.merge(&a);
        assert_eq!(chunk_hash(&ab), chunk_hash(&ba));

        // Points outside the camera frame are dropped, so the histogram holds
        // at most (and usually close to) the plotted sample count.
        let total: f64 = ab.data.iter().map(|c| c[3]).sum();
        assert!(total > 0.0 && total <= 2.0 * 5_000.0);
    }

    #[test]
    fn chunk_seed_matches_spec() {
        // Independent re-derivation of the chunk_seed formula.
        let challenge = challenge_from_seed(7);
        let mut hasher = Sha256::new();
        hasher.update(challenge);
        hasher.update(3u32.to_le_bytes());
        let digest = hasher.finalize();
        let mut first8 = [0u8; 8];
        first8.copy_from_slice(&digest[0..8]);
        assert_eq!(chunk_seed(&challenge, 3), u64::from_le_bytes(first8));
    }

    #[test]
    fn hex_roundtrip() {
        let c = challenge_from_seed(42);
        let hex = to_hex(&c);
        assert_eq!(hex.len(), 64);
        assert_eq!(challenge_from_hex(&hex).unwrap(), c);
        assert!(challenge_from_hex("abc").is_err());
    }
}
