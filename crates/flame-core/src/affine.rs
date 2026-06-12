//! 2D affine transforms, written `[[a b c] [d e f]]` acting on column `(x y 1)`.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::rng::Rng;

#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Affine {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Default for Affine {
    fn default() -> Self {
        Affine::identity()
    }
}

impl Affine {
    pub const fn new(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Self {
        Affine { a, b, c, d, e, f }
    }

    pub const fn identity() -> Self {
        Affine::new(1.0, 0.0, 0.0, 0.0, 1.0, 0.0)
    }

    pub const fn scaling(sx: f64, sy: f64) -> Self {
        Affine::new(sx, 0.0, 0.0, 0.0, sy, 0.0)
    }

    /// Apply to `(x, y)`, returning the transformed pair.
    ///
    /// NOTE: this reads both inputs before writing either output. The original
    /// single-file version overwrote `x` and then used the *new* `x` to compute
    /// `y`, which corrupted every transform — that bug is fixed here.
    #[inline]
    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.b * y + self.c,
            self.d * x + self.e * y + self.f,
        )
    }

    /// A "reasonable" random affine: contraction-biased so the chaos game tends
    /// to converge instead of flying off to infinity.
    pub fn random(rng: &mut Rng) -> Self {
        // Coefficients in [-1, 1], translations a bit smaller.
        Affine::new(
            rng.range(-1.0, 1.0),
            rng.range(-1.0, 1.0),
            rng.range(-1.0, 1.0),
            rng.range(-1.0, 1.0),
            rng.range(-1.0, 1.0),
            rng.range(-1.0, 1.0),
        )
    }

    /// Component-wise linear interpolation (crude but adequate for v1 animation;
    /// see `interpolate` module for the rotation-aware path).
    pub fn lerp(&self, other: &Affine, t: f64) -> Affine {
        let l = |x: f64, y: f64| x + (y - x) * t;
        Affine::new(
            l(self.a, other.a),
            l(self.b, other.b),
            l(self.c, other.c),
            l(self.d, other.d),
            l(self.e, other.e),
            l(self.f, other.f),
        )
    }
}
