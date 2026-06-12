//! The non-linear "variations" from Draves & Reckase, *The Fractal Flame
//! Algorithm*. Each variation maps a point to a point; a transform blends
//! several of them by weight.
//!
//! Conventions (matching the paper / flam3):
//!   r²    = x² + y²
//!   r     = sqrt(r²)
//!   theta = atan2(x, y)   (angle from the +y axis)
//!   phi   = atan2(y, x)   (angle from the +x axis)

use core::f64::consts::PI;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::fmath;
use crate::rng::Rng;

/// Parameter-free variations. (Parametric ones like `fan`/`rings`/`blob` can be
/// added later once genomes carry per-variation parameters.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Variation {
    Linear,
    Sinusoidal,
    Spherical,
    Swirl,
    Horseshoe,
    Polar,
    Handkerchief,
    Heart,
    Disc,
    Spiral,
    Hyperbolic,
    Diamond,
    Ex,
    Julia,
    Exponential,
    Power,
    Cosine,
    Eyefish,
    Bubble,
    Cylinder,
    Tangent,
    Cross,
}

impl Variation {
    /// Every variation, in declaration order.
    pub const ALL: [Variation; 22] = [
        Variation::Linear,
        Variation::Sinusoidal,
        Variation::Spherical,
        Variation::Swirl,
        Variation::Horseshoe,
        Variation::Polar,
        Variation::Handkerchief,
        Variation::Heart,
        Variation::Disc,
        Variation::Spiral,
        Variation::Hyperbolic,
        Variation::Diamond,
        Variation::Ex,
        Variation::Julia,
        Variation::Exponential,
        Variation::Power,
        Variation::Cosine,
        Variation::Eyefish,
        Variation::Bubble,
        Variation::Cylinder,
        Variation::Tangent,
        Variation::Cross,
    ];

    #[inline]
    pub fn index(self) -> usize {
        Variation::ALL.iter().position(|&v| v == self).unwrap()
    }

    /// Apply the (unweighted) variation to `(x, y)`.
    #[inline]
    pub fn apply(self, x: f64, y: f64, rng: &mut Rng) -> (f64, f64) {
        // Precompute the common scalars.
        let r2 = x * x + y * y;
        let r = r2.sqrt();
        let theta = fmath::atan2(x, y); // angle from +y
        // Guard against division by zero on the degenerate origin point.
        let inv_r = if r > 1e-12 { 1.0 / r } else { 0.0 };

        match self {
            Variation::Linear => (x, y),
            Variation::Sinusoidal => (fmath::sin(x), fmath::sin(y)),
            Variation::Spherical => {
                let s = 1.0 / (r2 + 1e-12);
                (s * x, s * y)
            }
            Variation::Swirl => {
                let (s, c) = fmath::sincos(r2);
                (x * s - y * c, x * c + y * s)
            }
            Variation::Horseshoe => (inv_r * (x - y) * (x + y), inv_r * 2.0 * x * y),
            Variation::Polar => (theta / PI, r - 1.0),
            Variation::Handkerchief => (r * fmath::sin(theta + r), r * fmath::cos(theta - r)),
            Variation::Heart => (r * fmath::sin(theta * r), -r * fmath::cos(theta * r)),
            Variation::Disc => {
                let t = theta / PI;
                let (s, c) = fmath::sincos(PI * r);
                (t * s, t * c)
            }
            Variation::Spiral => (
                inv_r * (fmath::cos(theta) + fmath::sin(r)),
                inv_r * (fmath::sin(theta) - fmath::cos(r)),
            ),
            Variation::Hyperbolic => (fmath::sin(theta) * inv_r, r * fmath::cos(theta)),
            Variation::Diamond => (fmath::sin(theta) * fmath::cos(r), fmath::cos(theta) * fmath::sin(r)),
            Variation::Ex => {
                let p0 = fmath::sin(theta + r);
                let p1 = fmath::cos(theta - r);
                let (p0, p1) = (p0 * p0 * p0, p1 * p1 * p1);
                (r * (p0 + p1), r * (p0 - p1))
            }
            Variation::Julia => {
                let sqrt_r = r.sqrt();
                let omega = if rng.chance(0.5) { 0.0 } else { PI };
                let a = theta / 2.0 + omega;
                (sqrt_r * fmath::cos(a), sqrt_r * fmath::sin(a))
            }
            Variation::Exponential => {
                let e = fmath::exp(x - 1.0);
                let (s, c) = fmath::sincos(PI * y);
                (e * c, e * s)
            }
            Variation::Power => {
                let (s, c) = fmath::sincos(theta);
                let m = fmath::pow(r, s);
                (m * c, m * s)
            }
            Variation::Cosine => {
                let (s, c) = fmath::sincos(PI * x);
                (c * fmath::cosh(y), -s * fmath::sinh(y))
            }
            Variation::Eyefish => {
                let s = 2.0 / (r + 1.0);
                (s * x, s * y)
            }
            Variation::Bubble => {
                let s = 4.0 / (r2 + 4.0);
                (s * x, s * y)
            }
            Variation::Cylinder => (fmath::sin(x), y),
            Variation::Tangent => (fmath::sin(x) / fmath::cos(y), fmath::tan(y)),
            Variation::Cross => {
                let d = x * x - y * y;
                let s = (1.0 / (d * d + 1e-12)).sqrt();
                (s * x, s * y)
            }
        }
    }
}
