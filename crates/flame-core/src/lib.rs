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
pub mod render;
pub mod rng;
pub mod variations;

pub use affine::Affine;
pub use animate::animated;
pub use breed::{breed, crossover, mutate, BREED_MUTATION_RATE};
#[cfg(feature = "serde")]
pub use canonical::{canonical_json, canonicalize, sheep_id, sheep_id_hex};
pub use chunked::{
    challenge_from_hex, challenge_from_seed, chunk_hash, chunk_hash_hex, chunk_seed, render_chunk,
    render_proof_frame, sha256_hex, to_hex, Challenge, CHUNK_BURN_IN,
};
pub use genome::{Camera, Genome, Transform};
pub use palette::{Palette, Stop};
pub use render::{accumulate, iterate, render, tonemap, Accum, RenderOpts};
pub use rng::Rng;
pub use variations::Variation;
