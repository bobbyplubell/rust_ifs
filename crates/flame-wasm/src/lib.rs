//! Browser bindings for the flame renderer.
//!
//! The site ships genome JSON; this renders it in-browser via the exact same
//! `flame-core` code as native (CPU chaos game + tone mapping), so a
//! `(genome, seed)` renders byte-identical everywhere.

use flame_core::chunked;
use flame_core::genome::Genome;
use flame_core::render::{render, tonemap, Accum, RenderOpts};
use wasm_bindgen::prelude::*;

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

/// Render a genome (as JSON) to an RGBA8 byte buffer (`width*height*4`), ready
/// to drop into a canvas `ImageData`.
///
/// `rotate` is added to the camera angle so the gallery can animate a spin by
/// calling this each frame with an increasing value.
#[wasm_bindgen]
pub fn render_rgba(
    genome_json: &str,
    width: usize,
    height: usize,
    ss: usize,
    samples: u32,
    seed: u32,
    rotate: f64,
) -> Result<Vec<u8>, JsValue> {
    let mut genome: Genome = serde_json::from_str(genome_json)
        .map_err(|e| JsValue::from_str(&format!("bad genome json: {e}")))?;
    genome.camera.rotate += rotate;

    let opts = RenderOpts {
        width,
        height,
        ss,
        samples: samples as u64,
        burn_in: 20,
        seed: seed as u64,
    };
    Ok(render(&genome, &opts))
}

/// Render one animation frame: the genome at loop `phase` (0..1) — flam3-style
/// transform-basis rotation plus palette drift, with temporal samples (motion
/// blur): the budget is split over `temporal` sub-phases spanning `shutter`
/// loop-phase units (temporal <= 1 or shutter <= 0 = single instant).
/// Display-only; proofs always render the base genome.
#[wasm_bindgen]
pub fn render_frame(
    genome_json: &str,
    phase: f64,
    width: usize,
    height: usize,
    ss: usize,
    samples: u32,
    seed: u32,
    shutter: f64,
    temporal: u32,
) -> Result<Vec<u8>, JsValue> {
    let genome: Genome = serde_json::from_str(genome_json)
        .map_err(|e| JsValue::from_str(&format!("bad genome json: {e}")))?;
    let opts = RenderOpts {
        width,
        height,
        ss,
        samples: samples as u64,
        burn_in: 20,
        seed: seed as u64,
    };
    if temporal > 1 && shutter > 0.0 {
        Ok(flame_core::animate::render_motion(&genome, phase, shutter, temporal, &opts))
    } else {
        let g = flame_core::animate::animated(&genome, phase);
        Ok(render(&g, &opts))
    }
}

// ---- genetics: canonical JSON, sheep_id, breeding ---------------------------

fn parse_genome(json: &str, what: &str) -> Result<Genome, JsValue> {
    let genome: Genome = serde_json::from_str(json)
        .map_err(|e| JsValue::from_str(&format!("bad {what} genome json: {e}")))?;
    genome
        .validate()
        .map_err(|e| JsValue::from_str(&format!("invalid {what} genome: {e}")))?;
    Ok(genome)
}

/// Decode a 32-byte challenge hex string and derive the breeding rng seed
/// (u64 from the first 8 bytes, little-endian).
fn challenge_seed(challenge_hex: &str) -> Result<u64, JsValue> {
    let hex = challenge_hex.trim();
    if hex.len() != 64 {
        return Err(JsValue::from_str(&format!(
            "challenge must be 64 hex chars (32 bytes), got {}",
            hex.len()
        )));
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| JsValue::from_str("challenge is not valid hex"))?;
    }
    Ok(u64::from_le_bytes(bytes[0..8].try_into().unwrap()))
}

/// Re-serialize genome JSON into its canonical byte form.
#[wasm_bindgen]
pub fn canonicalize(genome_json: &str) -> Result<String, JsValue> {
    let genome = parse_genome(genome_json, "input")?;
    Ok(flame_core::canonical::canonical_json(&genome))
}

/// SHA-256 of the canonical genome JSON, as lowercase hex.
#[wasm_bindgen]
pub fn sheep_id(genome_json: &str) -> Result<String, JsValue> {
    let genome = parse_genome(genome_json, "input")?;
    Ok(flame_core::canonical::sheep_id_hex(&genome))
}

/// Deterministically breed two genomes. The rng seed is the first 8 bytes
/// (little-endian) of the decoded 32-byte challenge; mutation rate is 0.15.
/// Returns the child's canonical JSON.
#[wasm_bindgen]
pub fn breed(a_json: &str, b_json: &str, challenge_hex: &str) -> Result<String, JsValue> {
    let a = parse_genome(a_json, "parent a")?;
    let b = parse_genome(b_json, "parent b")?;
    let seed = challenge_seed(challenge_hex)?;
    let child = flame_core::breed::breed(&a, &b, seed);
    child
        .validate()
        .map_err(|e| JsValue::from_str(&format!("bred child failed validation: {e}")))?;
    Ok(flame_core::canonical::canonical_json(&child))
}

/// Mutate a genome with the given per-site rate, seeded from the challenge
/// like `breed`. Returns the mutant's canonical JSON.
#[wasm_bindgen]
pub fn mutate_genome(genome_json: &str, challenge_hex: &str, rate: f64) -> Result<String, JsValue> {
    if !rate.is_finite() || !(0.0..=1.0).contains(&rate) {
        return Err(JsValue::from_str("rate must be in [0, 1]"));
    }
    let mut genome = parse_genome(genome_json, "input")?;
    let seed = challenge_seed(challenge_hex)?;
    let mut rng = flame_core::rng::Rng::new(seed);
    flame_core::breed::mutate(&mut genome, &mut rng, rate);
    genome
        .validate()
        .map_err(|e| JsValue::from_str(&format!("mutant failed validation: {e}")))?;
    Ok(flame_core::canonical::canonical_json(&genome))
}

/// A random genome (same generator as `flame dump`), as canonical JSON.
#[wasm_bindgen]
pub fn random_genome_json(seed: u32, transforms: u32) -> Result<String, JsValue> {
    if !(1..=8).contains(&transforms) {
        return Err(JsValue::from_str("transforms must be in 1..=8"));
    }
    let mut rng = flame_core::rng::Rng::new(seed as u64);
    let genome = Genome::random(&mut rng, transforms as usize);
    Ok(flame_core::canonical::canonical_json(&genome))
}

// ---- chunked (progressive / provable) rendering -----------------------------

fn parse_genome_unvalidated(genome_json: &str) -> Result<Genome, JsValue> {
    serde_json::from_str(genome_json)
        .map_err(|e| JsValue::from_str(&format!("bad genome json: {e}")))
}

fn parse_challenge(challenge_hex: &str) -> Result<chunked::Challenge, JsValue> {
    chunked::challenge_from_hex(challenge_hex).map_err(|e| JsValue::from_str(&e))
}

/// A progressive, provable render: genome + spec + the running accumulation
/// buffer live in wasm memory. Each `render_chunk(idx)` renders chunk `idx`
/// into a temporary buffer, hashes it (the render-proof unit), merges it into
/// the running sum, and returns the hex hash. `tonemap()` can be called at any
/// point for the current progressive image.
#[wasm_bindgen]
pub struct ChunkedRender {
    genome: Genome,
    width: u32,
    height: u32,
    ss: u32,
    samples_per_chunk: u32,
    n_chunks: u32,
    challenge: chunked::Challenge,
    running: Accum,
    chunks_done: u32,
}

#[wasm_bindgen]
impl ChunkedRender {
    #[wasm_bindgen(constructor)]
    pub fn new(
        genome_json: &str,
        width: u32,
        height: u32,
        ss: u32,
        samples_per_chunk: u32,
        n_chunks: u32,
        challenge_hex: &str,
    ) -> Result<ChunkedRender, JsValue> {
        if width == 0 || height == 0 || ss == 0 {
            return Err(JsValue::from_str("width/height/ss must be nonzero"));
        }
        let genome = parse_genome_unvalidated(genome_json)?;
        let challenge = parse_challenge(challenge_hex)?;
        let running = Accum::new((width * ss) as usize, (height * ss) as usize);
        Ok(ChunkedRender {
            genome,
            width,
            height,
            ss,
            samples_per_chunk,
            n_chunks,
            challenge,
            running,
            chunks_done: 0,
        })
    }

    /// Render chunk `idx` into its own buffer, merge it into the running
    /// accumulation, and return the chunk's hex hash.
    pub fn render_chunk(&mut self, idx: u32) -> Result<String, JsValue> {
        if idx >= self.n_chunks {
            return Err(JsValue::from_str(&format!(
                "chunk idx {idx} out of range (n_chunks = {})",
                self.n_chunks
            )));
        }
        let chunk = chunked::render_chunk(
            &self.genome,
            self.width as usize,
            self.height as usize,
            self.ss as usize,
            self.samples_per_chunk as u64,
            &self.challenge,
            idx,
        );
        let hash = chunked::chunk_hash_hex(&chunk);
        self.running.merge(&chunk);
        self.chunks_done += 1;
        Ok(hash)
    }

    /// Tone-map the current running accumulation to RGBA8 (`width*height*4`).
    pub fn tonemap(&self) -> Vec<u8> {
        tonemap(
            &self.running,
            &self.genome,
            self.width as usize,
            self.height as usize,
            self.ss as usize,
        )
    }

    pub fn chunks_done(&self) -> u32 {
        self.chunks_done
    }
}

/// One frame of a protocol-v3 loop proof: hash (the proof unit) plus the
/// tone-mapped RGBA of that frame, so rendering your proof doubles as
/// watching the loop (and the frames can be cached for replay).
#[wasm_bindgen]
pub struct ProofFrame {
    hash: String,
    rgba: Vec<u8>,
}

#[wasm_bindgen]
impl ProofFrame {
    #[wasm_bindgen(getter)]
    pub fn hash(&self) -> String {
        self.hash.clone()
    }
    #[wasm_bindgen(getter)]
    pub fn rgba(&self) -> Vec<u8> {
        self.rgba.clone()
    }
}

#[wasm_bindgen]
pub fn proof_frame(
    genome_json: &str,
    width: u32,
    height: u32,
    ss: u32,
    samples_per_frame: u32,
    challenge_hex: &str,
    idx: u32,
    n_frames: u32,
    temporal: u32,
) -> Result<ProofFrame, JsValue> {
    let genome = parse_genome_unvalidated(genome_json)?;
    let challenge = parse_challenge(challenge_hex)?;
    let accum = chunked::render_proof_frame(
        &genome, width as usize, height as usize, ss as usize,
        samples_per_frame as u64, &challenge, idx, n_frames, temporal,
    );
    let hash = chunked::chunk_hash_hex(&accum);
    let rgba = tonemap(&accum, &genome, width as usize, height as usize, ss as usize);
    Ok(ProofFrame { hash, rgba })
}

/// Audit one loop-proof frame: recompute its hash only (no pixels kept).
#[wasm_bindgen]
pub fn audit_frame(
    genome_json: &str,
    width: u32,
    height: u32,
    ss: u32,
    samples_per_frame: u32,
    challenge_hex: &str,
    idx: u32,
    n_frames: u32,
    temporal: u32,
) -> Result<String, JsValue> {
    let genome = parse_genome_unvalidated(genome_json)?;
    let challenge = parse_challenge(challenge_hex)?;
    let accum = chunked::render_proof_frame(
        &genome, width as usize, height as usize, ss as usize,
        samples_per_frame as u64, &challenge, idx, n_frames, temporal,
    );
    Ok(chunked::chunk_hash_hex(&accum))
}

/// Re-render one chunk and return its hex hash without keeping any pixels —
/// the audit primitive (1/n_chunks of a render's cost).
#[wasm_bindgen]
pub fn audit_chunk(
    genome_json: &str,
    width: u32,
    height: u32,
    ss: u32,
    samples_per_chunk: u32,
    challenge_hex: &str,
    idx: u32,
) -> Result<String, JsValue> {
    let genome = parse_genome_unvalidated(genome_json)?;
    let challenge = parse_challenge(challenge_hex)?;
    let chunk = chunked::render_chunk(
        &genome,
        width as usize,
        height as usize,
        ss as usize,
        samples_per_chunk as u64,
        &challenge,
        idx,
    );
    Ok(chunked::chunk_hash_hex(&chunk))
}

/// Convenience challenge for casual (non-proof) renders:
/// `sha256(le64(seed))`, returned as lowercase hex.
#[wasm_bindgen]
pub fn challenge_from_seed(seed: u32) -> String {
    chunked::to_hex(&chunked::challenge_from_seed(seed as u64))
}
