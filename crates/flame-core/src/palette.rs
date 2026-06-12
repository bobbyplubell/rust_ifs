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

    /// A random palette with `n` interior stops plus locked endpoints.
    pub fn random(rng: &mut Rng) -> Palette {
        let n = 2 + rng.below(4); // 2..=5 control colors
        let mut stops = Vec::with_capacity(n);
        for i in 0..n {
            let pos = i as f64 / (n as f64 - 1.0);
            stops.push(Stop {
                pos,
                rgb: [rng.f64(), rng.f64(), rng.f64()],
            });
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
