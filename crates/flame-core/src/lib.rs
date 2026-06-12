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
pub mod genome;
pub mod interpolate;
pub mod palette;
pub mod render;
pub mod rng;
pub mod variations;

pub use affine::Affine;
pub use genome::{Camera, Genome, Transform};
pub use palette::{Palette, Stop};
pub use render::{iterate, render, RenderOpts};
pub use rng::Rng;
pub use variations::Variation;
