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

// ---- chunked (progressive / provable) rendering -----------------------------

fn parse_genome(genome_json: &str) -> Result<Genome, JsValue> {
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
        let genome = parse_genome(genome_json)?;
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
    let genome = parse_genome(genome_json)?;
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
