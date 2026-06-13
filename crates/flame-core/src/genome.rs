//! A *genome* is the complete description of one flame: the transforms (the
//! IFS), the camera, the palette, and the tone-mapping parameters. This is the
//! unit that mutates/breeds and the unit the website ships as JSON.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::affine::Affine;
use crate::fmath;
use crate::palette::Palette;
use crate::rng::Rng;
use crate::variations::{pval, Variation, N_PVALS};
use core::f64::consts::PI;

/// One IFS map: pre-affine -> weighted blend of variations -> post-affine,
/// carrying a color coordinate.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Transform {
    /// Selection probability weight (relative; normalized at use).
    pub weight: f64,
    /// Color coordinate this transform pulls the running color toward, in [0, 1].
    pub color: f64,
    /// Pre-transform affine.
    pub affine: Affine,
    /// Post-transform affine (applied after the variation blend).
    pub post: Affine,
    /// Per-variation blend weights, indexed by `Variation::index`.
    pub variations: Vec<f64>,
    /// Parameter block for the parametric variations (see `variations::pval`).
    pub pvals: [f64; N_PVALS],
    /// How strongly this transform pulls the color coordinate toward its own
    /// color: `c <- c*(1-s) + color*s`. 0.5 is the classic Draves blend;
    /// 0 leaves color untouched (REQUIRED for symmetry transforms, per the
    /// paper sec. 7, or colors wash out).
    pub color_speed: f64,
}

impl Transform {
    /// Apply this transform to a working point + color, in place.
    #[inline]
    pub fn apply(&self, x: &mut f64, y: &mut f64, color: &mut f64, rng: &mut Rng) {
        // Pre affine.
        let (px, py) = self.affine.apply(*x, *y);
        // Weighted blend of variations evaluated at the same pre-affine point.
        let mut bx = 0.0;
        let mut by = 0.0;
        for (i, &w) in self.variations.iter().enumerate() {
            if w == 0.0 {
                continue;
            }
            let (vx, vy) = Variation::ALL[i].apply(px, py, &self.pvals, &self.affine, w, rng);
            bx += w * vx;
            by += w * vy;
        }
        // Post affine.
        let (fx, fy) = self.post.apply(bx, by);
        *x = fx;
        *y = fy;
        // Draves color blending, weighted by color_speed (0.5 = classic).
        let s = self.color_speed;
        *color = *color * (1.0 - s) + self.color * s;
    }

    /// Random parameter block with sensible ranges for every parametric
    /// variation (harmless when the variation is unused; meaningful when
    /// mutation later enables one).
    pub fn random_pvals(rng: &mut Rng) -> [f64; N_PVALS] {
        let mut p = [0.0; N_PVALS];
        let sign = if rng.chance(0.5) { 1.0 } else { -1.0 };
        p[pval::JULIAN_POWER] = sign * (2.0 + rng.below(4) as f64);
        p[pval::JULIAN_DIST] = if rng.chance(0.7) { 1.0 } else { rng.range(0.5, 2.0) };
        let sign = if rng.chance(0.5) { 1.0 } else { -1.0 };
        p[pval::JULIASCOPE_POWER] = sign * (2.0 + rng.below(4) as f64);
        p[pval::JULIASCOPE_DIST] = if rng.chance(0.7) { 1.0 } else { rng.range(0.5, 2.0) };
        p[pval::BLOB_LOW] = rng.range(0.3, 0.8);
        p[pval::BLOB_HIGH] = rng.range(0.9, 1.6);
        p[pval::BLOB_WAVES] = (2 + rng.below(7)) as f64;
        p[pval::CURL_C1] = rng.range(-0.8, 0.8);
        p[pval::CURL_C2] = rng.range(-0.8, 0.8);
        p[pval::FAN2_X] = rng.range(-1.0, 1.0);
        p[pval::FAN2_Y] = rng.range(-1.0, 1.0);
        p[pval::RINGS2_VAL] = rng.range(0.3, 1.2);
        p[pval::PDJ_A] = rng.range(-2.5, 2.5);
        p[pval::PDJ_B] = rng.range(-2.5, 2.5);
        p[pval::PDJ_C] = rng.range(-2.5, 2.5);
        p[pval::PDJ_D] = rng.range(-2.5, 2.5);
        p[pval::PERSP_ANGLE] = rng.range(0.2, 1.3);
        p[pval::PERSP_DIST] = rng.range(1.0, 3.0);
        p[pval::RADIAL_BLUR_ANGLE] = rng.range(-1.0, 1.0);
        p[pval::PIE_SLICES] = (3 + rng.below(6)) as f64;
        p[pval::PIE_ROTATION] = rng.range(-PI, PI);
        p[pval::PIE_THICKNESS] = rng.range(0.3, 0.9);
        p[pval::NGON_POWER] = rng.range(1.5, 4.5);
        p[pval::NGON_SIDES] = (3 + rng.below(6)) as f64;
        p[pval::NGON_CORNERS] = rng.range(1.0, 3.0);
        p[pval::NGON_CIRCLE] = rng.range(1.0, 3.0);
        p[pval::RECT_X] = rng.range(0.2, 1.2);
        p[pval::RECT_Y] = rng.range(0.2, 1.2);
        p
    }

    /// Variations that work as a transform's dominant "shape" — biased toward
    /// the ones that produce locally-2D (solid-looking) attractor regions.
    const SHAPE_POOL: [Variation; 18] = [
        Variation::JuliaN,
        Variation::JuliaN, // double weight: the classic solid look
        Variation::JuliaScope,
        Variation::Spherical,
        Variation::Blob,
        Variation::Curl,
        Variation::Eyefish,
        Variation::Bubble,
        Variation::Disc,
        Variation::Fan2,
        Variation::Rings2,
        Variation::Pdj,
        Variation::Swirl,
        Variation::Julia,
        Variation::Ngon,
        Variation::Fisheye,
        Variation::Rings,
        Variation::Fan,
    ];

    pub fn random(rng: &mut Rng) -> Transform {
        // One dominant shape variation + a linear floor keeps structure
        // legible and solid instead of uniform-random gauze.
        let mut variations = vec![0.0; Variation::ALL.len()];
        let shape = Self::SHAPE_POOL[rng.below(Self::SHAPE_POOL.len())];
        variations[shape.index()] = rng.range(0.6, 1.1);
        variations[Variation::Linear.index()] += rng.range(0.1, 0.45);
        if rng.chance(0.3) {
            let extra = rng.below(Variation::ALL.len());
            variations[extra] += rng.range(0.05, 0.3);
        }
        // Mild contraction bias so trajectories settle instead of scattering.
        let mut affine = Affine::random(rng);
        let s = rng.range(0.55, 0.95);
        affine.a *= s;
        affine.b *= s;
        affine.d *= s;
        affine.e *= s;
        Transform {
            weight: rng.range(0.2, 1.0),
            color: rng.f64(),
            affine,
            post: Affine::identity(),
            variations,
            pvals: Self::random_pvals(rng),
            color_speed: 0.5,
        }
    }
}

/// Maps world space to the image. `scale` is image-pixels-per-world-unit at the
/// center; `rotate` spins the whole flame (handy for animation loops).
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Camera {
    pub center_x: f64,
    pub center_y: f64,
    pub scale: f64,
    pub rotate: f64,
}

impl Default for Camera {
    fn default() -> Self {
        Camera {
            center_x: 0.0,
            center_y: 0.0,
            scale: 0.25,
            rotate: 0.0,
        }
    }
}

impl Camera {
    /// Build the affine that maps world coordinates to pixel coordinates for an
    /// image of `width`x`height`. World units are scaled by `scale * min(w,h)`.
    pub fn world_to_image(&self, width: usize, height: usize) -> Affine {
        let s = self.scale * width.min(height) as f64;
        let (sin, cos) = fmath::sincos(self.rotate);
        // rotate+scale about the camera center, then move to image center.
        Affine::new(
            s * cos,
            -s * sin,
            width as f64 * 0.5 - s * (cos * self.center_x - sin * self.center_y),
            s * sin,
            s * cos,
            height as f64 * 0.5 - s * (sin * self.center_x + cos * self.center_y),
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Genome {
    pub transforms: Vec<Transform>,
    pub final_transform: Option<Transform>,
    pub palette: Palette,
    pub camera: Camera,
    /// Overall brightness multiplier applied before tone mapping.
    pub brightness: f64,
    /// Display gamma (Draves' tone curve exponent).
    pub gamma: f64,
    /// 0 = per-channel gamma, 1 = gamma on luminance only. Mixes the two.
    pub vibrancy: f64,
    /// Background color (linear RGB) the flame is composited over.
    pub background: [f64; 3],
}

impl Genome {
    /// Frame the camera on the attractor: probe-iterate with a FIXED seed,
    /// fit the 5th-95th percentile bounding box, and set center/scale with
    /// some margin. Deterministic (fixed probe seed, exact ops only), so
    /// every peer frames a bred child identically. Display-affecting only —
    /// proofs hash whatever the genome's camera sees, framed or not.
    pub fn auto_frame(&mut self) {
        const PROBE: u64 = 0xF7A3E;
        const N: usize = 30_000;
        let mut xs = Vec::with_capacity(N);
        let mut ys = Vec::with_capacity(N);
        crate::render::iterate(self, N as u64, 30, PROBE, |x, y, _| {
            if x.abs() < 1e4 && y.abs() < 1e4 {
                xs.push(x);
                ys.push(y);
            }
        });
        if xs.len() < N / 4 {
            return; // mostly escaping: leave the default camera
        }
        xs.sort_unstable_by(f64::total_cmp);
        ys.sort_unstable_by(f64::total_cmp);
        let lo = xs.len() * 5 / 100;
        let hi = xs.len() * 95 / 100;
        let (x0, x1) = (xs[lo], xs[hi.min(xs.len() - 1)]);
        let (y0, y1) = (ys[lo], ys[hi.min(ys.len() - 1)]);
        let span = (x1 - x0).max(y1 - y0).max(1e-3);
        self.camera.center_x = 0.5 * (x0 + x1);
        self.camera.center_y = 0.5 * (y0 + y1);
        // scale is image-pixels-per-world-unit relative to min(w,h): fill
        // ~78% of the frame with the percentile box.
        self.camera.scale = 0.78 / span;
        self.camera.rotate = 0.0;
    }

    /// Pick a transform index by weight using a uniform draw.
    #[inline]
    pub fn pick(&self, rng: &mut Rng) -> usize {
        let total: f64 = self.transforms.iter().map(|t| t.weight).sum();
        let mut r = rng.range(0.0, total);
        for (i, t) in self.transforms.iter().enumerate() {
            r -= t.weight;
            if r <= 0.0 {
                return i;
            }
        }
        self.transforms.len() - 1
    }

    /// A random genome with `n` transforms. Priors are tuned for the classic
    /// solid-object look: dominant-shape transforms, occasional rotational
    /// symmetry, flam3's gamma-4 tone curve.
    pub fn random(rng: &mut Rng, n: usize) -> Genome {
        let mut transforms: Vec<Transform> = (0..n).map(|_| Transform::random(rng)).collect();

        // Symmetry per the paper (sec. 7): each symmetry transform gets
        // weight equal to the SUM of all other weights (half the jumps cross
        // arms — equal-density branches, not shadows), and color_speed 0 so
        // the color coordinate is untouched (else colors wash out).
        if rng.chance(0.45) && n + 3 <= 8 {
            let base_sum: f64 = transforms.iter().map(|t| t.weight).sum();
            let mut symmetry = |affine: Affine, rng: &mut Rng| {
                let mut variations = vec![0.0; Variation::ALL.len()];
                variations[Variation::Linear.index()] = 1.0;
                Transform {
                    weight: base_sum,
                    color: rng.f64(), // irrelevant at color_speed 0
                    affine,
                    post: Affine::identity(),
                    variations,
                    pvals: Transform::random_pvals(rng),
                    color_speed: 0.0,
                }
            };
            if rng.chance(0.25) {
                // Dihedral: mirror across the y axis.
                let t = symmetry(Affine::new(-1.0, 0.0, 0.0, 0.0, 1.0, 0.0), rng);
                transforms.push(t);
            } else {
                let order = 2 + rng.below(3); // 2..=4 fold rotational
                for k in 1..order {
                    let theta = k as f64 / order as f64 * core::f64::consts::TAU;
                    let t = symmetry(Affine::identity().rotated(theta), rng);
                    transforms.push(t);
                }
            }
        }

        let final_transform = if rng.chance(0.35) {
            Some(Transform::random(rng))
        } else {
            None
        };
        let mut genome = Genome {
            transforms,
            final_transform,
            palette: Palette::random(rng),
            camera: Camera::default(),
            brightness: 1.0,
            gamma: 4.0,
            vibrancy: 1.0,
            background: [0.0, 0.0, 0.0],
        };
        genome.auto_frame();
        genome
    }
}
