//! Genome interpolation — the basis of animated "sheep".
//!
//! flam3-style: smooth **Catmull-Rom splines** through a cyclic sequence of
//! keyframe genomes (not just pairwise lerp), with **rotation-aware** affine
//! interpolation: each pre/post affine's linear part is decomposed into a
//! rotation angle plus residual, angles are unwrapped along the shortest arc
//! across keys and splined separately, so transforms *turn* between keyframes
//! instead of collapsing through degenerate matrices.
//!
//! Keyframes must share a shape (same transform count, same final-transform
//! presence, same palette stop count) — the CLI generates compatible keys.
//! Display/CLI-only: nothing here is part of the render protocol.

use crate::affine::Affine;
use crate::fmath;
use crate::genome::{Camera, Genome, Transform};
use crate::variations::N_PVALS;

#[inline]
fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// Catmull-Rom through p1..p2 with neighbors p0, p3 at local parameter u.
#[inline]
fn cr(p0: f64, p1: f64, p2: f64, p3: f64, u: f64) -> f64 {
    let u2 = u * u;
    let u3 = u2 * u;
    0.5 * ((2.0 * p1)
        + (-p0 + p2) * u
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * u2
        + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * u3)
}

/// Rotation-aware affine decomposition: angle of the first basis column plus
/// the residual matrix expressed in the de-rotated frame.
struct AffineDecomp {
    angle: f64,
    // residual linear part (R(-angle) * L) and translation
    ra: f64,
    rb: f64,
    rd: f64,
    re: f64,
    c: f64,
    f: f64,
}

fn decompose(a: &Affine) -> AffineDecomp {
    let angle = fmath::atan2(a.d, a.a);
    let (s, c) = fmath::sincos(angle);
    // R(-angle) * [[a b], [d e]]
    AffineDecomp {
        angle,
        ra: c * a.a + s * a.d,
        rb: c * a.b + s * a.e,
        rd: -s * a.a + c * a.d,
        re: -s * a.b + c * a.e,
        c: a.c,
        f: a.f,
    }
}

fn recompose(d: &AffineDecomp) -> Affine {
    let (s, c) = fmath::sincos(d.angle);
    Affine::new(
        c * d.ra - s * d.rd,
        c * d.rb - s * d.re,
        d.c,
        s * d.ra + c * d.rd,
        s * d.rb + c * d.re,
        d.f,
    )
}

/// Unwrap `next` to within pi of `prev` (shortest arc continuity).
fn unwrap_angle(prev: f64, next: f64) -> f64 {
    let mut a = next;
    while a - prev > core::f64::consts::PI {
        a -= core::f64::consts::TAU;
    }
    while prev - a > core::f64::consts::PI {
        a += core::f64::consts::TAU;
    }
    a
}

fn spline_affine(keys: [&Affine; 4], u: f64) -> Affine {
    let mut d: Vec<AffineDecomp> = keys.iter().map(|a| decompose(a)).collect();
    for i in 1..4 {
        d[i].angle = unwrap_angle(d[i - 1].angle, d[i].angle);
    }
    recompose(&AffineDecomp {
        angle: cr(d[0].angle, d[1].angle, d[2].angle, d[3].angle, u),
        ra: cr(d[0].ra, d[1].ra, d[2].ra, d[3].ra, u),
        rb: cr(d[0].rb, d[1].rb, d[2].rb, d[3].rb, u),
        rd: cr(d[0].rd, d[1].rd, d[2].rd, d[3].rd, u),
        re: cr(d[0].re, d[1].re, d[2].re, d[3].re, u),
        c: cr(d[0].c, d[1].c, d[2].c, d[3].c, u),
        f: cr(d[0].f, d[1].f, d[2].f, d[3].f, u),
    })
}

fn spline_transform(keys: [&Transform; 4], u: f64) -> Transform {
    let k = |f: &dyn Fn(&Transform) -> f64| cr(f(keys[0]), f(keys[1]), f(keys[2]), f(keys[3]), u);
    let nvar = keys[1].variations.len();
    let mut variations = Vec::with_capacity(nvar);
    for i in 0..nvar {
        variations.push(k(&|t| t.variations.get(i).copied().unwrap_or(0.0)).max(0.0));
    }
    let mut pvals = [0.0; N_PVALS];
    for (i, v) in pvals.iter_mut().enumerate() {
        *v = k(&|t| t.pvals[i]);
    }
    let nx = keys[1].xaos.len();
    let mut xaos = Vec::with_capacity(nx);
    for i in 0..nx {
        xaos.push(k(&|t| t.xaos.get(i).copied().unwrap_or(1.0)).max(0.0));
    }
    Transform {
        weight: k(&|t| t.weight).max(0.05),
        color: k(&|t| t.color).clamp(0.0, 1.0),
        affine: spline_affine([&keys[0].affine, &keys[1].affine, &keys[2].affine, &keys[3].affine], u),
        post: spline_affine([&keys[0].post, &keys[1].post, &keys[2].post, &keys[3].post], u),
        variations,
        pvals,
        color_speed: k(&|t| t.color_speed).clamp(0.0, 1.0),
        xaos,
    }
}

/// Genome at position `t` in `[0, 1)` along a smooth cyclic spline through
/// `keys` (≥ 2 genomes of identical shape).
pub fn spline_loop(keys: &[Genome], t: f64) -> Genome {
    assert!(keys.len() >= 2, "spline_loop needs at least 2 keyframes");
    let k = keys.len();
    let t = t.rem_euclid(1.0) * k as f64;
    let seg = (t as usize).min(k - 1);
    let u = t - seg as f64;
    let at = |i: isize| &keys[(seg as isize + i).rem_euclid(k as isize) as usize];
    let (g0, g1, g2, g3) = (at(-1), at(0), at(1), at(2));

    let n = g1.transforms.len();
    let mut transforms = Vec::with_capacity(n);
    for i in 0..n {
        transforms.push(spline_transform(
            [
                &g0.transforms[i % g0.transforms.len()],
                &g1.transforms[i],
                &g2.transforms[i % g2.transforms.len()],
                &g3.transforms[i % g3.transforms.len()],
            ],
            u,
        ));
    }
    let final_transform = match (&g0.final_transform, &g1.final_transform, &g2.final_transform, &g3.final_transform) {
        (Some(f0), Some(f1), Some(f2), Some(f3)) => Some(spline_transform([f0, f1, f2, f3], u)),
        _ => g1.final_transform.clone(),
    };

    let sc = |f: &dyn Fn(&Genome) -> f64| cr(f(g0), f(g1), f(g2), f(g3), u);
    let mut palette = g1.palette.clone();
    if [g0, g2, g3].iter().all(|g| g.palette.stops.len() == palette.stops.len()) {
        for (i, stop) in palette.stops.iter_mut().enumerate() {
            for ch in 0..3 {
                stop.rgb[ch] = cr(
                    g0.palette.stops[i].rgb[ch],
                    g1.palette.stops[i].rgb[ch],
                    g2.palette.stops[i].rgb[ch],
                    g3.palette.stops[i].rgb[ch],
                    u,
                )
                .clamp(0.0, 1.0);
            }
        }
    }

    Genome {
        transforms,
        final_transform,
        palette,
        camera: Camera {
            center_x: sc(&|g| g.camera.center_x),
            center_y: sc(&|g| g.camera.center_y),
            scale: sc(&|g| g.camera.scale).max(1e-3),
            rotate: sc(&|g| g.camera.rotate),
        },
        brightness: sc(&|g| g.brightness).max(0.1),
        gamma: sc(&|g| g.gamma).max(0.5),
        vibrancy: sc(&|g| g.vibrancy).clamp(0.0, 1.0),
        background: [
            sc(&|g| g.background[0]).clamp(0.0, 1.0),
            sc(&|g| g.background[1]).clamp(0.0, 1.0),
            sc(&|g| g.background[2]).clamp(0.0, 1.0),
        ],
    }
}

/// Pairwise interpolation, kept for compatibility: a 2-key spline at segment
/// position `t` (now rotation-aware rather than naive coefficient lerp).
impl Genome {
    pub fn lerp(&self, other: &Genome, t: f64) -> Genome {
        // Map [0,1] across the first segment of the 2-key cycle.
        spline_loop(core::slice::from_ref(self).iter().chain([other]).cloned().collect::<Vec<_>>().as_slice(), t * 0.5)
    }
}

#[allow(unused)]
fn _quiet_lints(t: f64) -> f64 {
    lerp(t, t, t)
}
