//! Sheep animation, the flam3 way.
//!
//! The original Electric Sheep's motion is not a camera move: each transform's
//! affine *basis* rotates through 2π over the loop, so the attractor swirls
//! and morphs through itself and returns exactly to its start. `animated` is a
//! pure function of `(genome, phase)` — deterministic like everything else, so
//! any phase of any sheep's loop is recomputable by anyone.
//!
//! Animation is display-only: votes/proofs always render the base genome
//! (phase 0); `animated` is never part of the protocol surface.

use crate::genome::Genome;

/// The genome at loop phase `t` (`0.0..1.0`; periodic, `animated(g, 0) == g`
/// up to float identity at exactly 0). The final transform, camera, palette
/// and tone parameters stay fixed; only the IFS bases rotate.
pub fn animated(genome: &Genome, phase: f64) -> Genome {
    let theta = phase * std::f64::consts::TAU;
    let mut g = genome.clone();
    for t in g.transforms.iter_mut() {
        t.affine = t.affine.rotated(theta);
        // Classic palette drift: cycle each transform's color coordinate once
        // around the palette per loop (periodic, plain arithmetic).
        t.color = (t.color + phase).rem_euclid(1.0);
    }
    if let Some(ft) = g.final_transform.as_mut() {
        ft.color = (ft.color + phase).rem_euclid(1.0);
    }
    g
}

/// Render one animation frame with flam3-style **temporal samples** (motion
/// blur): the sample budget is split across `steps` sub-phases spanning
/// `shutter` (in loop-phase units), all accumulated into ONE histogram before
/// the log-density tone map. Linear-space accumulation then log mapping is
/// what gives original Electric Sheep its smooth motion; a single-instant
/// frame looks strobed by comparison. Display-only (proofs are static).
pub fn render_motion(
    genome: &crate::genome::Genome,
    phase: f64,
    shutter: f64,
    steps: u32,
    opts: &crate::render::RenderOpts,
) -> Vec<u8> {
    let steps = steps.max(1);
    let mut accum = crate::render::Accum::new(opts.width * opts.ss, opts.height * opts.ss);
    let per_step = (opts.samples / steps as u64).max(1);
    for k in 0..steps {
        let p = phase + shutter * (k as f64 / steps as f64);
        let g = animated(genome, p);
        crate::render::accumulate(&g, per_step, opts.burn_in, opts.seed ^ (k as u64), &mut accum);
    }
    crate::render::tonemap(&accum, genome, opts.width, opts.height, opts.ss)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    #[test]
    fn phase_zero_is_near_identity() {
        let mut rng = Rng::new(7);
        let g = Genome::random(&mut rng, 3);
        let a = animated(&g, 0.0);
        for (x, y) in g.transforms.iter().zip(a.transforms.iter()) {
            assert!((x.affine.a - y.affine.a).abs() < 1e-12);
            assert!((x.affine.e - y.affine.e).abs() < 1e-12);
        }
    }

    #[test]
    fn loop_closes() {
        let mut rng = Rng::new(3);
        let g = Genome::random(&mut rng, 3);
        let a = animated(&g, 1.0);
        for (x, y) in g.transforms.iter().zip(a.transforms.iter()) {
            assert!((x.affine.a - y.affine.a).abs() < 1e-9);
            assert!((x.affine.b - y.affine.b).abs() < 1e-9);
        }
    }
}
