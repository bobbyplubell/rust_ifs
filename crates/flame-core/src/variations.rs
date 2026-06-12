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
        let theta = x.atan2(y); // angle from +y
        // Guard against division by zero on the degenerate origin point.
        let inv_r = if r > 1e-12 { 1.0 / r } else { 0.0 };

        match self {
            Variation::Linear => (x, y),
            Variation::Sinusoidal => (x.sin(), y.sin()),
            Variation::Spherical => {
                let s = 1.0 / (r2 + 1e-12);
                (s * x, s * y)
            }
            Variation::Swirl => {
                let (s, c) = r2.sin_cos();
                (x * s - y * c, x * c + y * s)
            }
            Variation::Horseshoe => (inv_r * (x - y) * (x + y), inv_r * 2.0 * x * y),
            Variation::Polar => (theta / PI, r - 1.0),
            Variation::Handkerchief => (r * (theta + r).sin(), r * (theta - r).cos()),
            Variation::Heart => (r * (theta * r).sin(), -r * (theta * r).cos()),
            Variation::Disc => {
                let t = theta / PI;
                let (s, c) = (PI * r).sin_cos();
                (t * s, t * c)
            }
            Variation::Spiral => (inv_r * (theta.cos() + r.sin()), inv_r * (theta.sin() - r.cos())),
            Variation::Hyperbolic => (theta.sin() * inv_r, r * theta.cos()),
            Variation::Diamond => (theta.sin() * r.cos(), theta.cos() * r.sin()),
            Variation::Ex => {
                let p0 = (theta + r).sin();
                let p1 = (theta - r).cos();
                let (p0, p1) = (p0 * p0 * p0, p1 * p1 * p1);
                (r * (p0 + p1), r * (p0 - p1))
            }
            Variation::Julia => {
                let sqrt_r = r.sqrt();
                let omega = if rng.chance(0.5) { 0.0 } else { PI };
                let a = theta / 2.0 + omega;
                (sqrt_r * a.cos(), sqrt_r * a.sin())
            }
            Variation::Exponential => {
                let e = (x - 1.0).exp();
                let (s, c) = (PI * y).sin_cos();
                (e * c, e * s)
            }
            Variation::Power => {
                let (s, c) = theta.sin_cos();
                let m = r.powf(s);
                (m * c, m * s)
            }
            Variation::Cosine => {
                let (s, c) = (PI * x).sin_cos();
                (c * y.cosh(), -s * y.sinh())
            }
            Variation::Eyefish => {
                let s = 2.0 / (r + 1.0);
                (s * x, s * y)
            }
            Variation::Bubble => {
                let s = 4.0 / (r2 + 4.0);
                (s * x, s * y)
            }
            Variation::Cylinder => (x.sin(), y),
            Variation::Tangent => (x.sin() / y.cos(), y.tan()),
            Variation::Cross => {
                let d = x * x - y * y;
                let s = (1.0 / (d * d + 1e-12)).sqrt();
                (s * x, s * y)
            }
        }
    }
}
