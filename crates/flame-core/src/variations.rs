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
    // -- remainder of the paper's 49-variation catalog --
    Bent,
    Waves,       // dependent: affine b, c, e, f
    Fisheye,
    Popcorn,     // dependent: affine c, f
    Rings,       // dependent: affine c
    Fan,         // dependent: affine c, f
    Perspective, // parametric
    Noise,       // random
    Blur,        // random
    Gaussian,    // random
    RadialBlur,  // parametric + weight-dependent + random
    Pie,         // parametric + random
    Ngon,        // parametric
    Rectangles,  // parametric
    Arch,        // weight-dependent + random
    Square,      // random
    Rays,        // weight-dependent + random
    Blade,       // weight-dependent + random
    Secant,      // weight-dependent
    Twintrian,   // weight-dependent + random
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
    pub const PERSP_ANGLE: usize = 16;
    pub const PERSP_DIST: usize = 17;
    pub const RADIAL_BLUR_ANGLE: usize = 18;
    pub const PIE_SLICES: usize = 19;
    pub const PIE_ROTATION: usize = 20;
    pub const PIE_THICKNESS: usize = 21;
    pub const NGON_POWER: usize = 22;
    pub const NGON_SIDES: usize = 23;
    pub const NGON_CORNERS: usize = 24;
    pub const NGON_CIRCLE: usize = 25;
    pub const RECT_X: usize = 26;
    pub const RECT_Y: usize = 27;
}

/// Number of parameter slots per transform.
pub const N_PVALS: usize = 28;

impl Variation {
    /// Every variation, in declaration order.
    pub const ALL: [Variation; 49] = [
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
        Variation::Bent,
        Variation::Waves,
        Variation::Fisheye,
        Variation::Popcorn,
        Variation::Rings,
        Variation::Fan,
        Variation::Perspective,
        Variation::Noise,
        Variation::Blur,
        Variation::Gaussian,
        Variation::RadialBlur,
        Variation::Pie,
        Variation::Ngon,
        Variation::Rectangles,
        Variation::Arch,
        Variation::Square,
        Variation::Rays,
        Variation::Blade,
        Variation::Secant,
        Variation::Twintrian,
    ];

    #[inline]
    pub fn index(self) -> usize {
        Variation::ALL.iter().position(|&v| v == self).unwrap()
    }

    /// Apply the (unweighted) variation to `(x, y)`.
    ///
    /// Parametric variations read `pvals` (see `pval`); *dependent* ones read
    /// the transform's pre-affine coefficients; *weight-dependent* ones read
    /// their own blend weight `w` (the paper's v_ij) — all per the catalog.
    #[inline]
    pub fn apply(
        self,
        x: f64,
        y: f64,
        pvals: &[f64; N_PVALS],
        affine: &crate::affine::Affine,
        w: f64,
        rng: &mut Rng,
    ) -> (f64, f64) {
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
                // Paper V33: Lambda is an independent random sign.
                let power = nonzero(pvals[pval::JULIASCOPE_POWER]);
                let dist = pvals[pval::JULIASCOPE_DIST];
                let k = (power.abs() * rng.f64()).trunc();
                let phi = fmath::atan2(y, x);
                let lam = if rng.chance(0.5) { 1.0 } else { -1.0 };
                let t = (lam * phi + 2.0 * PI * k) / power;
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
                (pr * c, pr * s) // paper V23: (cos, sin)
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
                // Paper V25: t = theta + p2 - p1*trunc(2*theta*p2 / p1).
                let p1 = PI * (pvals[pval::FAN2_X] * pvals[pval::FAN2_X]) + 1e-10;
                let p2 = pvals[pval::FAN2_Y];
                let t = theta + p2 - p1 * (2.0 * theta * p2 / p1).trunc();
                let a = if t > p1 * 0.5 { theta - p1 * 0.5 } else { theta + p1 * 0.5 };
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
            Variation::Bent => {
                let nx = if x < 0.0 { 2.0 * x } else { x };
                let ny = if y < 0.0 { y * 0.5 } else { y };
                (nx, ny)
            }
            Variation::Waves => (
                x + affine.b * fmath::sin(y / (affine.c * affine.c + 1e-10)),
                y + affine.e * fmath::sin(x / (affine.f * affine.f + 1e-10)),
            ),
            Variation::Fisheye => {
                // Paper V16: note the reversed order of x and y.
                let s = 2.0 / (r + 1.0);
                (s * y, s * x)
            }
            Variation::Popcorn => (
                x + affine.c * fmath::sin(fmath::tan(3.0 * y)),
                y + affine.f * fmath::sin(fmath::tan(3.0 * x)),
            ),
            Variation::Rings => {
                let c2 = affine.c * affine.c + 1e-10;
                let t = (r + c2).rem_euclid(2.0 * c2) - c2 + r * (1.0 - c2);
                let (s, c) = fmath::sincos(theta);
                (t * c, t * s) // paper V21: (cos, sin)
            }
            Variation::Fan => {
                let t = PI * (affine.c * affine.c) + 1e-10;
                let half = t * 0.5;
                let a = if (theta + affine.f).rem_euclid(t) > half {
                    theta - half
                } else {
                    theta + half
                };
                let (s, c) = fmath::sincos(a);
                (r * c, r * s) // paper V22: (cos, sin)
            }
            Variation::Perspective => {
                let p1 = pvals[pval::PERSP_ANGLE];
                let p2 = pvals[pval::PERSP_DIST];
                let k = p2 / (p2 - y * fmath::sin(p1) + 1e-10);
                (k * x, k * y * fmath::cos(p1))
            }
            Variation::Noise => {
                let p1 = rng.f64();
                let (s, c) = fmath::sincos(2.0 * PI * rng.f64());
                (p1 * x * c, p1 * y * s)
            }
            Variation::Blur => {
                let p1 = rng.f64();
                let (s, c) = fmath::sincos(2.0 * PI * rng.f64());
                (p1 * c, p1 * s)
            }
            Variation::Gaussian => {
                let sum = rng.f64() + rng.f64() + rng.f64() + rng.f64() - 2.0;
                let (s, c) = fmath::sincos(2.0 * PI * rng.f64());
                (sum * c, sum * s)
            }
            Variation::RadialBlur => {
                // Paper V36 — weight-dependent: t1 contains v36, and the
                // result is divided by v36 (the blend multiplies it back).
                let p1 = pvals[pval::RADIAL_BLUR_ANGLE] * (PI / 2.0);
                let v = if w.abs() < 1e-9 { 1e-9 } else { w };
                let t1 = v * (rng.f64() + rng.f64() + rng.f64() + rng.f64() - 2.0);
                let phi = fmath::atan2(y, x);
                let t2 = phi + t1 * fmath::sin(p1);
                let t3 = t1 * fmath::cos(p1) - 1.0;
                let (s2, c2) = fmath::sincos(t2);
                ((r * c2 + t3 * x) / v, (r * s2 + t3 * y) / v)
            }
            Variation::Pie => {
                let p1 = pvals[pval::PIE_SLICES].max(1.0);
                let p2 = pvals[pval::PIE_ROTATION];
                let p3 = pvals[pval::PIE_THICKNESS];
                let t1 = (rng.f64() * p1 + 0.5).trunc();
                let t2 = p2 + (2.0 * PI / p1) * (t1 + rng.f64() * p3);
                let p = rng.f64();
                let (s, c) = fmath::sincos(t2);
                (p * c, p * s)
            }
            Variation::Ngon => {
                let p1 = pvals[pval::NGON_POWER];
                let sides = pvals[pval::NGON_SIDES].max(1.0);
                let p2 = 2.0 * PI / sides;
                let p3 = pvals[pval::NGON_CORNERS];
                let p4 = pvals[pval::NGON_CIRCLE];
                let phi = fmath::atan2(y, x);
                let t3 = phi - p2 * (phi / p2).floor();
                let t4 = if t3 > p2 * 0.5 { t3 } else { t3 - p2 };
                let denom = fmath::pow(r, p1).max(1e-10);
                let k = (p3 * (1.0 / (fmath::cos(t4).abs() + 1e-10) - 1.0) + p4) / denom;
                (k * x, k * y)
            }
            Variation::Rectangles => {
                let p1 = pvals[pval::RECT_X];
                let p2 = pvals[pval::RECT_Y];
                let nx = if p1.abs() < 1e-10 { x } else { (2.0 * (x / p1).floor() + 1.0) * p1 - x };
                let ny = if p2.abs() < 1e-10 { y } else { (2.0 * (y / p2).floor() + 1.0) * p2 - y };
                (nx, ny)
            }
            Variation::Arch => {
                let a = rng.f64() * PI * w;
                let (s, c) = fmath::sincos(a);
                (fmath::sin(a), s * s / (c + 1e-10))
            }
            Variation::Square => (rng.f64() - 0.5, rng.f64() - 0.5),
            Variation::Rays => {
                let v = if w.abs() < 1e-9 { 1e-9 } else { w };
                let k = v * fmath::tan(rng.f64() * PI * v) / (r2 + 1e-10);
                (k * fmath::cos(x), k * fmath::sin(y))
            }
            Variation::Blade => {
                let a = rng.f64() * r * w;
                let (s, c) = fmath::sincos(a);
                (x * (c + s), x * (c - s))
            }
            Variation::Secant => {
                let v = if w.abs() < 1e-9 { 1e-9 } else { w };
                (x, 1.0 / (v * fmath::cos(v * r) + 1e-10))
            }
            Variation::Twintrian => {
                let a = rng.f64() * r * w;
                let (s, c) = fmath::sincos(a);
                let t = fmath::log((s * s).max(1e-300)) / core::f64::consts::LN_10 + c;
                (x * t, x * (t - PI * s))
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
