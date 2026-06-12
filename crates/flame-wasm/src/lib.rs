//! Browser bindings for the flame renderer.
//!
//! The site ships genome JSON; this renders it in-browser via the exact same
//! `flame-core` code as native (CPU chaos game + tone mapping), so a
//! `(genome, seed)` renders byte-identical everywhere.

use flame_core::genome::Genome;
use flame_core::render::{render, RenderOpts};
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
