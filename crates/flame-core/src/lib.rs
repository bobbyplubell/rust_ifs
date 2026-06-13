//! # flame-core
//!
//! Pure, deterministic, I/O-free implementation of the Draves *Fractal Flame*
//! algorithm: genomes, variations, the chaos game, log-density tone mapping,
//! and genome interpolation for animation.
//!
//! It depends on nothing platform-specific so the exact same code drives the
//! native renderer (CLI/server) and the wasm website. A given `(genome, seed)`
//! produces a byte-identical image on every target.

pub mod affine;
pub mod animate;
pub mod breed;
#[cfg(feature = "serde")]
pub mod canonical;
pub mod chunked;
pub mod fmath;
pub mod genome;
pub mod interpolate;
pub mod palette;
pub mod palettes_lib;
pub mod render;
pub mod rng;
pub mod variations;

pub use affine::Affine;
pub use animate::animated;
pub use breed::{breed, crossover, mutate, BREED_MUTATION_RATE};
#[cfg(feature = "serde")]
pub use canonical::{canonical_json, canonicalize, sheep_id, sheep_id_hex};
pub use chunked::{
    batch_seed, challenge_from_hex, challenge_from_seed, chunk_hash, chunk_hash_hex, chunk_seed,
    hist_hash, hist_hash_hex, render_batch, render_chunk, render_proof_frame, sha256_hex,
    subtract_ok, to_hex, total_count, Challenge, CHUNK_BURN_IN, N_FRAMES,
};
pub use genome::{Camera, Genome, Transform};
pub use palette::{Palette, Stop};
pub use render::{accumulate, iterate, render, tonemap, Accum, RenderOpts, BLUR_FP, COLOR_FP};
pub use rng::Rng;
pub use variations::Variation;
