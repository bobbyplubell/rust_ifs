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

/// The variation set. The last seven are *parametric*: they read from the
/// transform's fixed 16-slot parameter block (`Transform::pvals`, layout in
/// `PVAL` below). These are the workhorses of the classic "solid object"
/// Electric Sheep look — julian/juliascope especially.
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
    // -- parametric (read Transform::pvals) --
    JuliaN,
    JuliaScope,
    Blob,
    Curl,
    Fan2,
    Rings2,
    Pdj,
}

/// Layout of the per-transform parameter block (`Transform::pvals`).
pub mod pval {
    pub const JULIAN_POWER: usize = 0;
    pub const JULIAN_DIST: usize = 1;
    pub const JULIASCOPE_POWER: usize = 2;
    pub const JULIASCOPE_DIST: usize = 3;
    pub const BLOB_LOW: usize = 4;
    pub const BLOB_HIGH: usize = 5;
    pub const BLOB_WAVES: usize = 6;
    pub const CURL_C1: usize = 7;
    pub const CURL_C2: usize = 8;
    pub const FAN2_X: usize = 9;
    pub const FAN2_Y: usize = 10;
    pub const RINGS2_VAL: usize = 11;
    pub const PDJ_A: usize = 12;
    pub const PDJ_B: usize = 13;
    pub const PDJ_C: usize = 14;
    pub const PDJ_D: usize = 15;
}

/// Number of parameter slots per transform.
pub const N_PVALS: usize = 16;

impl Variation {
    /// Every variation, in declaration order.
    pub const ALL: [Variation; 29] = [
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
        Variation::JuliaN,
        Variation::JuliaScope,
        Variation::Blob,
        Variation::Curl,
        Variation::Fan2,
        Variation::Rings2,
        Variation::Pdj,
    ];

    #[inline]
    pub fn index(self) -> usize {
        Variation::ALL.iter().position(|&v| v == self).unwrap()
    }

    /// Apply the (unweighted) variation to `(x, y)`. Parametric variations
    /// read `pvals` (see `pval`); the rest ignore it.
    #[inline]
    pub fn apply(self, x: f64, y: f64, pvals: &[f64; N_PVALS], rng: &mut Rng) -> (f64, f64) {
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
            // Angle convention for the parametric polar variations: phi is the
            // angle from the +x axis (atan2(y, x)) like flam3 uses for julian.
            Variation::JuliaN => {
                let power = nonzero(pvals[pval::JULIAN_POWER]);
                let dist = pvals[pval::JULIAN_DIST];
                let k = (power.abs() * rng.f64()).trunc();
                let t = (fmath::atan2(y, x) + 2.0 * PI * k) / power;
                let m = fmath::pow(r, dist / power);
                let (s, c) = fmath::sincos(t);
                (m * c, m * s)
            }
            Variation::JuliaScope => {
                let power = nonzero(pvals[pval::JULIASCOPE_POWER]);
                let dist = pvals[pval::JULIASCOPE_DIST];
                let k = (power.abs() * rng.f64()).trunc();
                let phi = fmath::atan2(y, x);
                let dir = if (k as i64) & 1 == 1 { -phi } else { phi };
                let t = (2.0 * PI * k + dir) / power;
                let m = fmath::pow(r, dist / power);
                let (s, c) = fmath::sincos(t);
                (m * c, m * s)
            }
            Variation::Blob => {
                let low = pvals[pval::BLOB_LOW];
                let high = pvals[pval::BLOB_HIGH];
                let waves = pvals[pval::BLOB_WAVES];
                let pr = r * (low + 0.5 * (high - low) * (fmath::sin(waves * theta) + 1.0));
                let (s, c) = fmath::sincos(theta);
                (pr * s, pr * c)
            }
            Variation::Curl => {
                let c1 = pvals[pval::CURL_C1];
                let c2 = pvals[pval::CURL_C2];
                let re = 1.0 + c1 * x + c2 * (x * x - y * y);
                let im = c1 * y + 2.0 * c2 * x * y;
                let s = 1.0 / (re * re + im * im + 1e-12);
                ((x * re + y * im) * s, (y * re - x * im) * s)
            }
            Variation::Fan2 => {
                let fx = pvals[pval::FAN2_X];
                let fy = pvals[pval::FAN2_Y];
                let dx = PI * (fx * fx + 1e-10);
                let dx2 = 0.5 * dx;
                let t = theta + fy - dx * ((theta + fy) / dx).trunc();
                let a = if t > dx2 { theta - dx2 } else { theta + dx2 };
                let (s, c) = fmath::sincos(a);
                (r * s, r * c)
            }
            Variation::Rings2 => {
                let p = pvals[pval::RINGS2_VAL] * pvals[pval::RINGS2_VAL] + 1e-10;
                let t = r - 2.0 * p * ((r + p) / (2.0 * p)).trunc() + r * (1.0 - p);
                let (s, c) = fmath::sincos(theta);
                (t * s, t * c)
            }
            Variation::Pdj => {
                let a = pvals[pval::PDJ_A];
                let b = pvals[pval::PDJ_B];
                let c = pvals[pval::PDJ_C];
                let d = pvals[pval::PDJ_D];
                (
                    fmath::sin(a * y) - fmath::cos(b * x),
                    fmath::sin(c * x) - fmath::cos(d * y),
                )
            }
        }
    }
}

/// Clamp a parametric "power" away from zero (genomes are validated, but a
/// bred/mutated value could land near it).
#[inline]
fn nonzero(p: f64) -> f64 {
    if p.abs() < 1e-6 { 2.0 } else { p }
}
