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

// ---- batch primitives (the protocol's unit of work, v2) --------------------

/// Number of animation frames in a sheep's loop. Frame `f` is the genome
/// animated to phase `f / N_FRAMES`. PROTOCOL CONSTANT.
pub const N_FRAMES: u32 = 64;

/// Burn-in iterations per batch (each batch settles onto the attractor
/// independently, like a chunk).
pub const BATCH_BURN_IN: u64 = CHUNK_BURN_IN;

/// Per-batch PRNG seed:
/// `u64::from_le_bytes(sha256(sheep_id ‖ "b" ‖ le32(frame) ‖ le32(idx))[0..8])`.
///
/// A batch `(frame, idx)` of a sheep is a deterministic slice of that frame's
/// sample stream; the seed pins it so every peer who renders the same
/// `(sheep_id, frame, idx)` produces a byte-identical histogram.
pub fn batch_seed(sheep_id: &[u8], frame: u32, idx: u32) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(sheep_id);
    hasher.update(b"b");
    hasher.update(frame.to_le_bytes());
    hasher.update(idx.to_le_bytes());
    let digest = hasher.finalize();
    let mut first8 = [0u8; 8];
    first8.copy_from_slice(&digest[0..8]);
    u64::from_le_bytes(first8)
}

/// Render one batch into its own fresh integer accumulation buffer at
/// supersampled resolution (`w*ss x h*ss`).
///
/// The genome is animated to `phase = frame / N_FRAMES`, then `spp` samples are
/// plotted from `seed = batch_seed(sheep_id, frame, idx)`. Deterministic and
/// content-addressable: hash the returned `Accum` with `hist_hash` to get the
/// batch's commitment.
pub fn render_batch(
    genome: &Genome,
    sheep_id: &[u8],
    frame: u32,
    idx: u32,
    width: usize,
    height: usize,
    ss: usize,
    spp: u32,
) -> Accum {
    let mut accum = Accum::new(width * ss, height * ss);
    let phase = frame as f64 / N_FRAMES as f64;
    let g = crate::animate::animated(genome, phase);
    let seed = batch_seed(sheep_id, frame, idx);
    accumulate(&g, spp as u64, BATCH_BURN_IN, seed, &mut accum);
    accum
}

/// Total `count` over all cells (the verification "count conservation" left
/// side; equals the number of plotted samples for unweighted renders).
pub fn total_count(accum: &Accum) -> u64 {
    accum.data.iter().map(|c| c[3]).sum()
}

/// True iff subtracting `batch` from `acc` cell-by-cell would never underflow
/// any channel — i.e. every cell of `batch` is `<=` the corresponding cell of
/// `acc` (confirms `batch ⊆ acc`). Dimensions must match.
pub fn subtract_ok(acc: &Accum, batch: &Accum) -> bool {
    if acc.w != batch.w || acc.h != batch.h {
        return false;
    }
    acc.data
        .iter()
        .zip(batch.data.iter())
        .all(|(a, b)| a[0] >= b[0] && a[1] >= b[1] && a[2] >= b[2] && a[3] >= b[3])
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
/// running sum) — cells row-major, each cell serialized as 4 x u64
/// little-endian in order `[r_fixed, g_fixed, b_fixed, count]` (32 bytes/cell).
/// Integer cells make this hash content-addressable and order-independent
/// under merge (see `Accum`).
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

/// Histogram hash (alias of `chunk_hash`): SHA-256 over the integer cells, the
/// content address of a batch / accumulated render. `H(hist bytes)`.
pub fn hist_hash(accum: &Accum) -> [u8; 32] {
    chunk_hash(accum)
}

/// `hist_hash` as lowercase hex.
pub fn hist_hash_hex(accum: &Accum) -> String {
    chunk_hash_hex(accum)
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
                "02c15ce80a46a06a9ae280e20a2d65da4e32fb6df72469662a8ad7585f0c7c4b",
                "13a7278a93dcc8f599dd12fe55b860f6fcba8bd0ed5ab1fdeea8e627ad47a934",
                "8f965ffcd121cfc0f19a2d5273aed533ecea635ae1b725714f600956338939e0",
                "1dec812183612b2e0b4660924c74cb7b66a7d7fc82c5d56f3cb1531cf35fc623",
                "135ab69ab6b5e35078c3cca9bfefbe4e6086611ce4dc04f6e78ba8475deead61",
                "3d834d789a0bb1cfe9a078624665663d8dd211bfc7f8350959376b48b366775f",
                "e6127c9176c605aa83d5106e79aaef0d3ae673f8a1ff008661e1f337343bb921",
                "ba0295e310452d469193831c94d6da8451f897dfbdd03cd75981d12ecf4ac155",
            ],
            "7e54dd56fcc9c7a4e1f2570028a70c6623138dcdacbece4007d36e1e3ecef678",
        ),
        (
            3,
            [
                "0030d48ece6bc4b8d625d2769bb40551c64b33190e0e0f196d7af3fc89d857c8",
                "960dda54ec3b22596dbadcaf33a38f5287b6a5e1ce8339b0f4c20e8cc7c9b491",
                "694b96568d06b7362f7e8b22fe005fc21228a9997a5a9ea9f708f56172d1df7f",
                "d75117a417c2431b28efadefd7a02ce4fbd6e14771a74ad528c7978c8e9ee0e1",
                "af435f36b9642be76b257435c589d5cc0417eccdee591cd1d56565219a2c445d",
                "8b8bc0ee415649a4e024e261244395165032d3fc2a40fe3f6db64c75ae1c0cd8",
                "95b5ba3b482755887c5b240905058b0184048dd824aa91214eb4c6b305e3cfab",
                "930478fb4a66e25abf9f5e179c105e9bb51802d3fdec5fda79ca31a1cb9570e3",
            ],
            "82761af3d58e1f5c817cdb5f7c44c7f12f890bdddff9d2a2d2b4e224c4fb09d8",
        ),
        (
            7,
            [
                "b2ec82e9c05309b9ad26d36a60c51acdcc4187efbaddb757025aa5dfe12887f1",
                "7ec5ed9d5f5d5ccf3822d871ce9fa41c5bef9115e68c951cf826e7fa8fb283e7",
                "f3a6f3cb25aa9688b8acd5aefc38ff1faeae00278a683449bb48ae42ecfbe0d1",
                "3a9f4120de3db7ae428da5246ad1f2bba83bf037b4ff78fc01f7c0f40c5cec8d",
                "c49493d4a56ba61b5977663f1daea4e0312487b028da509087a455bdeb550dfb",
                "d11236dcbab27180773b9e41afdb6d9a18ee6111ef4c3564f872705f4c047d0a",
                "1c23211be511ec5d1c92532ab230f59950b8d4a40f5e1552486fbdf6e874b368",
                "6c47bc636c5f511dbd907339dbcbe37faf5b11d2ae277ecf80955625fde5dff1",
            ],
            "434ae957c1548356fd03d2bb13a35f7322465d5aa5109813ca4434c26bf38d7e",
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
        let total: u64 = ab.data.iter().map(|c| c[3]).sum();
        assert!(total > 0 && total <= 2 * 5_000);
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

    /// `render_batch` determinism + golden. Two fresh calls must produce a
    /// byte-identical hash (the property the protocol relies on: every peer
    /// rendering the same `(sheep_id, frame, idx)` gets the same bytes), and
    /// that hash is pinned so a future bitstream change is caught.
    ///
    /// sheep_id is the canonical id of a fixed genome; frame=2, idx=5, 64x64,
    /// ss=1, spp=50_000.
    const BATCH_GOLDEN: &str =
        "1635d61a57b67bd3e33f06d980c3f178f9a3c127b7978c61e8d50346c093bc73";

    #[test]
    fn render_batch_is_deterministic_and_golden() {
        let mut rng = Rng::new(2);
        let genome = Genome::random(&mut rng, 3);
        let sheep_id = crate::canonical::sheep_id(&genome);

        let a = render_batch(&genome, &sheep_id, 2, 5, 64, 64, 1, 50_000);
        let b = render_batch(&genome, &sheep_id, 2, 5, 64, 64, 1, 50_000);
        let ha = hist_hash_hex(&a);
        let hb = hist_hash_hex(&b);
        assert_eq!(ha, hb, "render_batch must be byte-deterministic");

        // Unweighted batch: total count == plotted samples landing in frame.
        assert!(total_count(&a) > 0 && total_count(&a) <= 50_000);
        // A batch is trivially a subset of itself.
        assert!(subtract_ok(&a, &b));

        assert_eq!(
            ha, BATCH_GOLDEN,
            "PROTOCOL BREAK: render_batch hash changed (sheep_id genome seed 2, frame 2, idx 5)"
        );
    }

    #[test]
    #[ignore = "generator for BATCH_GOLDEN; run with --ignored --nocapture"]
    fn print_batch_golden() {
        let mut rng = Rng::new(2);
        let genome = Genome::random(&mut rng, 3);
        let sheep_id = crate::canonical::sheep_id(&genome);
        let a = render_batch(&genome, &sheep_id, 2, 5, 64, 64, 1, 50_000);
        println!("BATCH_GOLDEN: \"{}\"", hist_hash_hex(&a));
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
