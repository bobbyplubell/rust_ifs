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

    /// The optimized iterate() (active lists + precomputed pick totals) must
    /// be bit-identical to the naive reference for genomes WITH active xaos
    /// (the golden corpus only exercises identity rows).
    #[test]
    fn xaos_fast_path_matches_reference() {
        let mut rng = Rng::new(11);
        let mut genome = Genome::random(&mut rng, 4);
        // Force a juicy xaos matrix.
        let n = genome.transforms.len();
        genome.fix_xaos();
        genome.transforms[0].xaos[1 % n] = 0.0;
        genome.transforms[1 % n].xaos[0] = 2.5;
        if n > 2 {
            genome.transforms[2].xaos[n - 1] = 0.3;
        }

        // Reference: the naive loop (genome.pick + Transform::apply).
        let mut reference = crate::render::Accum::new(64, 64);
        {
            let to_img = genome.camera.world_to_image(64, 64);
            let mut r = Rng::new(99);
            let mut x = r.range(-1.0, 1.0);
            let mut y = r.range(-1.0, 1.0);
            let mut color = r.f64();
            let mut prev: Option<usize> = None;
            for i in 0..120_000u64 {
                let t = genome.pick(prev, &mut r);
                prev = Some(t);
                genome.transforms[t].apply(&mut x, &mut y, &mut color, &mut r);
                if !x.is_finite() || !y.is_finite() {
                    x = r.range(-1.0, 1.0);
                    y = r.range(-1.0, 1.0);
                    color = r.f64();
                    continue;
                }
                if i < 20 {
                    continue;
                }
                let (mut px, mut py, mut pc) = (x, y, color);
                if let Some(ft) = &genome.final_transform {
                    ft.apply(&mut px, &mut py, &mut pc, &mut r);
                }
                let rgb = genome.palette.color(pc);
                let (ix, iy) = to_img.apply(px, py);
                if ix >= 0.0 && iy >= 0.0 && ix < 64.0 && iy < 64.0 {
                    reference.add(ix as usize, iy as usize, rgb);
                }
            }
        }
        let mut fast = crate::render::Accum::new(64, 64);
        crate::render::accumulate(&genome, 119_980, 20, 99, &mut fast);
        assert_eq!(
            chunk_hash_hex(&reference),
            chunk_hash_hex(&fast),
            "optimized iterate diverges from reference under xaos"
        );
    }

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
                "0c012bb64a0958d1d3b84ac4350151ab14832b5efdbe5eb90924a0885b042726",
                "31b1abe75cba33b326914dc5d8766f9b7f774c880f335057d27f51aa037e7b00",
                "38eda0ed150bdb85e09eb43570eab62fb3d105d014c1f94645833d018c75fac4",
                "05a3c22cfb83e911155ae5ed39bf91bbb2f77dad8bc1f2bd36acdecd0c911554",
                "63a7f790acd4b7ef24d81dbd47fd4b9a26f3da9797a757a13a0115fcee063a7d",
                "622413b897ba1c44fc8b5a235437587ebd3a5ded904560810073481ec48a94ac",
                "e7b50e360216f2a918da548d098a874cd7f65780fca06b1c5ed06d7d13265595",
                "ea3bab5e4b95df9440d71b6ac47038b73d87827be4e201ac66ca89fa9baa7fa3",
            ],
            "98f449900dec61b834a8ad2896558d57c88ee39d16fe39fdd7ab95ed45e82d63",
        ),
        (
            3,
            [
                "7aabd423279765bfd2c5893f76308f1b9bafb1aa8fcbdbd843c09e79ec79c63e",
                "8d77aaef051914be28cd289805932d1bb17b67093033805d9b34ee8dd2bc8923",
                "1c2ef43b6920222f37ab9ce661ccc2709dea71a4cba85a020183eb36898cf1de",
                "0aa00c8986efde44be7c0aecd00028f521f3e7294b6c919f7c588f8fc5c14d0d",
                "dbd6a2d6fbe41dc94d68b105f1696fb7aa21132ded5eeb4b6d06a188bdd10a38",
                "9536bfb6db8ef543163e250b436669da7108cb118068e71299a58296e002fc01",
                "0bf46dd782221a5c01a26593366186db74be273672c83c58f6e8cefdc8d54f4b",
                "64e20b4e4413ecba9660f317bc03d60d9649d79e7eda7b119aa2b8ee635e2919",
            ],
            "2c5b3661bcd0f49fd37b6248e24b92a6bfe2d3c3e410fecc08845b02658d4472",
        ),
        (
            7,
            [
                "51b3aa1a7813e4ff6a26d319e82af6c5ef883e919d68a37e6ed78882e875ca2a",
                "0bc3b7dde706ff3f637d867074ea788edebb6df5004fabe067f33ac51e25a49f",
                "751e2541ed82ac1a71d3e9ce391383fe40392421d51896ad513de6a0c233ae3a",
                "19f87e633a1635a5681e2a24b3affa9c77bd6f1d5be35e55480e2c5bb1b8603d",
                "59b7572a04cb8e622e85829a5b0129a49ee9c007e724b18f8e4a134c9f4aaf51",
                "3127798169fd783d3f7642af22811e7719a9b7a815bf65d7847a43fcf7316f6b",
                "bfb5c77e09b82399036f0344cc6679ac33aff676e926fc79b42e7b5c65f4fe63",
                "be612c60c0e78328a2e6bbe559a6995a5a8a35564b21a36dfc7f1a0003a926cf",
            ],
            "dd39b67d3f461918b1989c78dcf040dfdefd4f3ebd7542d3caa34a719e28d35d",
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
