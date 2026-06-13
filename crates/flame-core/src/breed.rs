//! Deterministic breeding operators: crossover + mutation.
//!
//! All randomness comes from the deterministic `Rng` (splitmix64), so a child
//! is a pure function of `(parent_a, parent_b, seed)` — every peer that breeds
//! the same pair with the same seed gets a byte-identical genome (and thus the
//! same sheep_id).

use crate::genome::{Genome, Transform};
use crate::rng::Rng;
use crate::variations::Variation;

/// Mutation rate used by [`breed`].
pub const BREED_MUTATION_RATE: f64 = 0.15;

/// Recombine two parent genomes into a child.
///
/// - Child transform count: a's or b's, 50/50.
/// - Transform slot `i`: cloned from a or b (50/50), at index
///   `i % parent.transforms.len()`.
/// - `final_transform`: from a or b (50/50; may be `None`).
/// - Palette: 50/50 take one parent's verbatim; otherwise lerp the two with
///   `t = rng.f64()` when stop counts match, else take one parent's.
/// - Camera, and each tone field (`brightness`, `gamma`, `vibrancy`,
///   `background`), independently from a or b (50/50).
pub fn crossover(a: &Genome, b: &Genome, rng: &mut Rng) -> Genome {
    let n = if rng.chance(0.5) {
        a.transforms.len()
    } else {
        b.transforms.len()
    };
    let mut transforms = Vec::with_capacity(n);
    for i in 0..n {
        let parent = if rng.chance(0.5) { a } else { b };
        transforms.push(parent.transforms[i % parent.transforms.len()].clone());
    }

    let final_transform = if rng.chance(0.5) {
        a.final_transform.clone()
    } else {
        b.final_transform.clone()
    };

    let palette = if rng.chance(0.5) {
        // Take one parent's palette verbatim.
        if rng.chance(0.5) {
            a.palette.clone()
        } else {
            b.palette.clone()
        }
    } else if a.palette.stops.len() == b.palette.stops.len() {
        let t = rng.f64();
        a.palette.lerp(&b.palette, t)
    } else if rng.chance(0.5) {
        a.palette.clone()
    } else {
        b.palette.clone()
    };

    let camera = if rng.chance(0.5) { a.camera } else { b.camera };
    let brightness = if rng.chance(0.5) { a.brightness } else { b.brightness };
    let gamma = if rng.chance(0.5) { a.gamma } else { b.gamma };
    let vibrancy = if rng.chance(0.5) { a.vibrancy } else { b.vibrancy };
    let background = if rng.chance(0.5) { a.background } else { b.background };

    let mut child = Genome {
        transforms,
        final_transform,
        palette,
        camera,
        brightness,
        gamma,
        vibrancy,
        background,
    };
    // Xaos rows came from parents with possibly different transform counts.
    child.fix_xaos();
    child
}

/// Jitter every coefficient of a transform's pre and post affines.
fn jitter_affines(t: &mut Transform, rng: &mut Rng) {
    for affine in [&mut t.affine, &mut t.post] {
        affine.a += rng.range(-0.15, 0.15);
        affine.b += rng.range(-0.15, 0.15);
        affine.c += rng.range(-0.15, 0.15);
        affine.d += rng.range(-0.15, 0.15);
        affine.e += rng.range(-0.15, 0.15);
        affine.f += rng.range(-0.15, 0.15);
    }
}

/// Per-transform mutations (applied to regular and final transforms alike).
fn mutate_transform(t: &mut Transform, rng: &mut Rng, rate: f64) {
    if rng.chance(rate) {
        jitter_affines(t, rng);
    }
    if rng.chance(rate) {
        t.weight = (t.weight + rng.range(-0.15, 0.15)).max(0.05);
        t.color = (t.color + rng.range(-0.15, 0.15)).clamp(0.0, 1.0);
    }
    if rng.chance(rate * 0.5) {
        t.color_speed = (t.color_speed + rng.range(-0.15, 0.15)).clamp(0.0, 1.0);
    }
    if rng.chance(rate * 0.5) && !t.xaos.is_empty() {
        // Mutate one transition weight: suppress, boost, or jitter.
        let j = rng.below(t.xaos.len());
        t.xaos[j] = match rng.below(3) {
            0 => 0.0,
            1 => 1.0,
            _ => (t.xaos[j] + rng.range(-0.5, 0.5)).max(0.0),
        };
    }
    if rng.chance(rate * 0.5) {
        // Re-roll one variation weight, same style as Transform::random.
        let idx = rng.below(t.variations.len());
        t.variations[idx] = rng.range(0.2, 1.0);
    }
    if rng.chance(rate) {
        // Jitter one parametric value; occasionally re-roll the whole block.
        if rng.chance(0.2) {
            t.pvals = Transform::random_pvals(rng);
        } else {
            let i = rng.below(t.pvals.len());
            t.pvals[i] += rng.range(-0.25, 0.25);
        }
    }
}

/// Mutate a genome in place. `rate` is the per-site mutation probability;
/// `rate = 0.0` leaves the genome untouched.
pub fn mutate(g: &mut Genome, rng: &mut Rng, rate: f64) {
    for t in &mut g.transforms {
        mutate_transform(t, rng, rate);
    }
    if let Some(t) = &mut g.final_transform {
        mutate_transform(t, rng, rate);
    }

    // Camera.
    if rng.chance(rate) {
        g.camera.scale *= rng.range(0.9, 1.1);
        g.camera.rotate += rng.range(-0.2, 0.2);
    }

    // Palette: usually jitter, occasionally swap to a fresh library palette.
    if rng.chance(rate) {
        if rng.chance(0.25) {
            g.palette = crate::palette::Palette::from_library(
                rng.below(crate::palettes_lib::N_LIBRARY));
        } else {
            for stop in &mut g.palette.stops {
                for ch in &mut stop.rgb {
                    *ch = (*ch + rng.range(-0.08, 0.08)).clamp(0.0, 1.0);
                }
            }
        }
    }

    // Structural: add or remove a transform, keeping the count in 1..=8.
    if rng.chance(rate * 0.3) {
        let n = g.transforms.len();
        let add = rng.chance(0.5);
        if (add && n < 8) || n <= 1 {
            g.transforms.push(Transform::random(rng));
        } else {
            let idx = rng.below(g.transforms.len());
            g.transforms.remove(idx);
            // Drop the removed transform's column from every xaos row.
            for t in &mut g.transforms {
                if idx < t.xaos.len() {
                    t.xaos.remove(idx);
                }
            }
        }
        g.fix_xaos();
    }
}

/// Deterministically breed two parents: crossover then mutation at
/// [`BREED_MUTATION_RATE`], one `Rng::new(seed)` threaded through both.
pub fn breed(a: &Genome, b: &Genome, seed: u64) -> Genome {
    let mut rng = Rng::new(seed);
    let mut child = crossover(a, b, &mut rng);
    mutate(&mut child, &mut rng, BREED_MUTATION_RATE);
    // Re-frame on the child's own attractor (deterministic probe), so bred
    // sheep arrive centered no matter where the parents' cameras pointed.
    child.auto_frame();
    child
}

impl Genome {
    /// Sanity-check the genome's shape and numeric health.
    pub fn validate(&self) -> Result<(), String> {
        let n = self.transforms.len();
        if !(1..=8).contains(&n) {
            return Err(format!("transform count {n} outside 1..=8"));
        }

        let check_transform = |t: &Transform, what: &str| -> Result<(), String> {
            if !t.weight.is_finite() || !t.color.is_finite() {
                return Err(format!("{what}: non-finite weight/color"));
            }
            if t.weight <= 0.0 {
                return Err(format!("{what}: weight {} not > 0", t.weight));
            }
            for affine in [&t.affine, &t.post] {
                for v in [affine.a, affine.b, affine.c, affine.d, affine.e, affine.f] {
                    if !v.is_finite() {
                        return Err(format!("{what}: non-finite affine coefficient"));
                    }
                }
            }
            if t.variations.len() != Variation::ALL.len() {
                return Err(format!(
                    "{what}: variations vec has {} entries, expected {}",
                    t.variations.len(),
                    Variation::ALL.len()
                ));
            }
            if t.variations.iter().any(|v| !v.is_finite()) {
                return Err(format!("{what}: non-finite variation weight"));
            }
            if t.pvals.iter().any(|v| !v.is_finite()) {
                return Err(format!("{what}: non-finite variation parameter"));
            }
            if !t.color_speed.is_finite() || !(0.0..=1.0).contains(&t.color_speed) {
                return Err(format!("{what}: color_speed {} outside [0, 1]", t.color_speed));
            }
            if t.xaos.iter().any(|v| !v.is_finite() || *v < 0.0) {
                return Err(format!("{what}: bad xaos entry"));
            }
            Ok(())
        };

        for (i, t) in self.transforms.iter().enumerate() {
            check_transform(t, &format!("transform {i}"))?;
            if t.xaos.len() != n {
                return Err(format!(
                    "transform {i}: xaos row has {} entries, expected {n}",
                    t.xaos.len()
                ));
            }
        }
        if let Some(t) = &self.final_transform {
            check_transform(t, "final transform")?;
        }

        if self.palette.stops.len() < 2 {
            return Err(format!(
                "palette has {} stops, needs at least 2",
                self.palette.stops.len()
            ));
        }
        for (i, stop) in self.palette.stops.iter().enumerate() {
            if !stop.pos.is_finite() || stop.rgb.iter().any(|c| !c.is_finite()) {
                return Err(format!("palette stop {i}: non-finite value"));
            }
        }

        for v in [
            self.camera.center_x,
            self.camera.center_y,
            self.camera.scale,
            self.camera.rotate,
        ] {
            if !v.is_finite() {
                return Err("camera: non-finite value".to_string());
            }
        }

        if !self.brightness.is_finite() || self.brightness <= 0.0 {
            return Err(format!("brightness {} not finite and > 0", self.brightness));
        }
        if !self.gamma.is_finite() || self.gamma <= 0.0 {
            return Err(format!("gamma {} not finite and > 0", self.gamma));
        }
        if !self.vibrancy.is_finite() {
            return Err("vibrancy: non-finite value".to_string());
        }
        if self.background.iter().any(|c| !c.is_finite()) {
            return Err("background: non-finite value".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random_genome(seed: u64, n: usize) -> Genome {
        let mut rng = Rng::new(seed);
        Genome::random(&mut rng, n)
    }

    #[test]
    fn breed_is_deterministic_for_same_seed() {
        let a = random_genome(7, 4);
        let b = random_genome(3, 5);
        let child1 = breed(&a, &b, 0xDEADBEEF);
        let child2 = breed(&a, &b, 0xDEADBEEF);
        assert_eq!(child1, child2);
        #[cfg(feature = "serde")]
        {
            use crate::canonical::canonical_json;
            assert_eq!(
                canonical_json(&child1),
                canonical_json(&child2),
                "same seed must give byte-identical canonical JSON"
            );
        }
    }

    #[test]
    fn breed_differs_for_different_seeds() {
        let a = random_genome(7, 4);
        let b = random_genome(3, 5);
        let child1 = breed(&a, &b, 1);
        let child2 = breed(&a, &b, 2);
        assert_ne!(child1, child2, "different seeds should give different children");
    }

    #[test]
    fn bred_children_from_random_corpus_all_validate() {
        let parents: Vec<Genome> = (0..8)
            .map(|i| random_genome(100 + i, 1 + (i as usize % 8)))
            .collect();
        for p in &parents {
            p.validate().expect("random parent should validate");
        }
        let mut seed = 0u64;
        for a in &parents {
            for b in &parents {
                seed += 1;
                let child = breed(a, b, seed * 0x9E37_79B9);
                child
                    .validate()
                    .unwrap_or_else(|e| panic!("child of seed {seed} failed validate: {e}"));
            }
        }
    }

    #[test]
    fn mutate_with_rate_zero_is_identity() {
        for seed in [2u64, 3, 7, 42] {
            let original = random_genome(seed, 4);
            let mut mutated = original.clone();
            let mut rng = Rng::new(seed.wrapping_mul(31));
            mutate(&mut mutated, &mut rng, 0.0);
            assert_eq!(original, mutated);
        }
    }

    #[test]
    fn mutate_keeps_transform_count_in_bounds() {
        let mut rng = Rng::new(99);
        for seed in 0..50u64 {
            // Stress both edges: 1-transform and 8-transform genomes.
            for n in [1usize, 8] {
                let mut g = random_genome(seed, n);
                mutate(&mut g, &mut rng, 1.0);
                assert!(
                    (1..=8).contains(&g.transforms.len()),
                    "count {} out of bounds",
                    g.transforms.len()
                );
                g.validate().expect("mutated genome should validate");
            }
        }
    }

    #[test]
    fn validate_rejects_bad_genomes() {
        let good = random_genome(7, 3);

        let mut g = good.clone();
        g.transforms.clear();
        assert!(g.validate().is_err());

        let mut g = good.clone();
        g.transforms[0].weight = 0.0;
        assert!(g.validate().is_err());

        let mut g = good.clone();
        g.transforms[0].affine.a = f64::NAN;
        assert!(g.validate().is_err());

        let mut g = good.clone();
        g.transforms[0].variations.pop();
        assert!(g.validate().is_err());

        let mut g = good.clone();
        g.palette.stops.truncate(1);
        assert!(g.validate().is_err());

        let mut g = good.clone();
        g.gamma = 0.0;
        assert!(g.validate().is_err());

        let mut g = good.clone();
        g.brightness = -1.0;
        assert!(g.validate().is_err());

        let mut g = good;
        g.camera.scale = f64::INFINITY;
        assert!(g.validate().is_err());
    }
}
