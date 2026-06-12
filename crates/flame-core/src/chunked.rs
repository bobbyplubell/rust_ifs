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
                "0d38849c8ddf1bf4780444c6401a421107b48513ed3cb48a53f193e6a680dbd2",
                "e9295d3aaa17474a89328dbebc0dfc282024bc505e9ffa4665c9bb95780d72da",
                "83c74307c895bc8e4ab36f5af191f49515c077f632165d24d6476df216d621be",
                "2ed9c7ecd9e1fa3ccd500813a0977f6e5a3819439bb3dd73330f76b461f3592d",
                "127cd9481a6071c0f306d2923cdb8c902715a5281b4ee688aa19f8afe406cabc",
                "17c756a79d054048fd428038874ddc0092537c852c031d8f8b6b9b2b08fc42d3",
                "2a280049680b7cf99cc311ba0598cdd82f425e49f6ff9905451fc833a1bc4fef",
                "e17f46e012cb9b36a3d46962f6961028761942d248b9929a3cdabf298e0fbf30",
            ],
            "afa54649d0bc8d9e5c1752b428c796063eb4c315f01a4757cc176d08efbe4b70",
        ),
        (
            3,
            [
                "390941a421c6d328ef7a196ef36c17ef88f7c9d89a6374adbb37955951f5bc79",
                "157aa1919cf20d99685eda55a0cd63076f72b25fdcbd20ea9b838638aeb743ad",
                "4ca97dfe5584e0c23efc03177ca1520d1605a1b36168d0dc93b46a543360b6ec",
                "713eceb852062ea6cf78660ae240903f055d5e989c283e12991d4fba2b5360ed",
                "6f9c8876888d3914432ae4782847b51ee5afe7e9cc64d90c3a5ddc8ce373cb9a",
                "ddfa7a8263552897da39b436e563b071aa746424493b59a9ffa20b11f70cdaaf",
                "cb2d99bb1e62a2ce3be3d2e873498e94c5d73d054998e7faec91fba873bc19bc",
                "34b63f15d2947b375980301a0de9a72f5e8ff191565c47cf0eeeaea64d02068f",
            ],
            "0f3e0211902b8f884b07b570e525b91ab4c632758ab6e294488fce91b8ea707f",
        ),
        (
            7,
            [
                "b2e40de93d993ee335cee678a35c5611a3848e4903130ee3d430898c31a43584",
                "04a1c0ae963b2dd6c7dd50ec511830c42c7d36ce1dfedae6de0d0914e17476c9",
                "004314ac5be600aa88dc0e0b4da5b12d999b94658989daf99d17c17e7fe87756",
                "d0829d0ce67eaeea50921d27aed40d86110f81d3a3f60aade14d2cfec0c9e7db",
                "6e3232d80838967204848aab057ee8694758edcbc031b879b404618f6de85a06",
                "b88aadea3fe563ae82b2ad41a64e5493e9c12691ab4e821019b64889ea122f16",
                "1b3c319235a7566a50559e88f6b943350f89703aef77708b99d7ad8ad1667378",
                "e138995722d1920a11190e29eb7f82785415542d54a548aaf03738196099e7bb",
            ],
            "1380944fc0100f64d0b0e960c673b82305fd8fd637a2b7ba37571b515ad647b4",
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
