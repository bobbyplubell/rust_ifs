//! Color palettes as a list of stops sampled along the color coordinate `c`
//! in [0, 1]. Stops (rather than a fixed 256-entry LUT) make palettes cheap to
//! mutate in the genetic algorithm and to interpolate between for animation.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::rng::Rng;

#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Stop {
    /// Position along the color coordinate, in [0, 1].
    pub pos: f64,
    /// Linear-space RGB, each channel in [0, 1].
    pub rgb: [f64; 3],
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Palette {
    /// Sorted by `pos`. Always has at least two stops at 0.0 and 1.0.
    pub stops: Vec<Stop>,
}

impl Palette {
    /// Sample the palette at color coordinate `c` (clamped to [0, 1]),
    /// returning linear RGB in [0, 1].
    pub fn color(&self, c: f64) -> [f64; 3] {
        let c = c.clamp(0.0, 1.0);
        // stops are sorted; find the bracketing pair.
        let mut lo = &self.stops[0];
        let mut hi = &self.stops[self.stops.len() - 1];
        for w in self.stops.windows(2) {
            if c >= w[0].pos && c <= w[1].pos {
                lo = &w[0];
                hi = &w[1];
                break;
            }
        }
        let span = (hi.pos - lo.pos).max(1e-12);
        let t = ((c - lo.pos) / span).clamp(0.0, 1.0);
        [
            lo.rgb[0] + (hi.rgb[0] - lo.rgb[0]) * t,
            lo.rgb[1] + (hi.rgb[1] - lo.rgb[1]) * t,
            lo.rgb[2] + (hi.rgb[2] - lo.rgb[2]) * t,
        ]
    }

    /// A palette from the embedded flam3 library (see `palettes_lib`),
    /// converted to 16 stops. `idx` wraps.
    pub fn from_library(idx: usize) -> Palette {
        let data = &crate::palettes_lib::LIBRARY[idx % crate::palettes_lib::N_LIBRARY];
        let stops = (0..16)
            .map(|i| Stop {
                pos: i as f64 / 15.0,
                rgb: [
                    data[i * 3] as f64 / 255.0,
                    data[i * 3 + 1] as f64 / 255.0,
                    data[i * 3 + 2] as f64 / 255.0,
                ],
            })
            .collect();
        Palette { stops }
    }

    /// A random palette: usually one of the 702 flam3 library palettes (the
    /// hand-curated originals), sometimes a cosine gradient (Quilez-style):
    /// `color(t) = a + b*cos(2pi*(c*t + d))` per channel — hue-correlated,
    /// smooth, with a built-in bright-to-dark ramp that reads as lighting.
    /// Uniform-random stops average into pastel mud; these don't.
    pub fn random(rng: &mut Rng) -> Palette {
        use crate::fmath;
        if rng.chance(0.7) {
            return Palette::from_library(rng.below(crate::palettes_lib::N_LIBRARY));
        }
        const N: usize = 8;
        let a = [rng.range(0.35, 0.6), rng.range(0.35, 0.6), rng.range(0.35, 0.6)];
        let b = [rng.range(0.25, 0.5), rng.range(0.25, 0.5), rng.range(0.25, 0.5)];
        let c = rng.range(0.6, 1.4); // shared frequency keeps hues correlated
        let d = [rng.f64(), rng.f64(), rng.f64()];
        let mut stops = Vec::with_capacity(N);
        for i in 0..N {
            let t = i as f64 / (N as f64 - 1.0);
            let mut rgb = [0.0; 3];
            for k in 0..3 {
                rgb[k] = (a[k] + b[k] * fmath::cos(core::f64::consts::TAU * (c * t + d[k])))
                    .clamp(0.0, 1.0);
            }
            stops.push(Stop { pos: t, rgb });
        }
        Palette { stops }
    }

    /// Interpolate two palettes (assumes matching stop counts; falls back to
    /// `self` if they differ — callers should interpolate only genomes of
    /// compatible shape).
    pub fn lerp(&self, other: &Palette, t: f64) -> Palette {
        if self.stops.len() != other.stops.len() {
            return self.clone();
        }
        let stops = self
            .stops
            .iter()
            .zip(other.stops.iter())
            .map(|(a, b)| Stop {
                pos: a.pos + (b.pos - a.pos) * t,
                rgb: [
                    a.rgb[0] + (b.rgb[0] - a.rgb[0]) * t,
                    a.rgb[1] + (b.rgb[1] - a.rgb[1]) * t,
                    a.rgb[2] + (b.rgb[2] - a.rgb[2]) * t,
                ],
            })
            .collect();
        Palette { stops }
    }
}
