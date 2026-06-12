//! A *genome* is the complete description of one flame: the transforms (the
//! IFS), the camera, the palette, and the tone-mapping parameters. This is the
//! unit that mutates/breeds and the unit the website ships as JSON.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::affine::Affine;
use crate::fmath;
use crate::palette::Palette;
use crate::rng::Rng;
use crate::variations::Variation;

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
            let (vx, vy) = Variation::ALL[i].apply(px, py, rng);
            bx += w * vx;
            by += w * vy;
        }
        // Post affine.
        let (fx, fy) = self.post.apply(bx, by);
        *x = fx;
        *y = fy;
        // Draves color blending: pull halfway toward this transform's color.
        *color = (*color + self.color) * 0.5;
    }

    pub fn random(rng: &mut Rng) -> Transform {
        // Pick 1..=3 active variations so flames stay legible.
        let active = 1 + rng.below(3);
        let mut variations = vec![0.0; Variation::ALL.len()];
        for _ in 0..active {
            let idx = rng.below(Variation::ALL.len());
            variations[idx] += rng.range(0.2, 1.0);
        }
        Transform {
            weight: rng.range(0.2, 1.0),
            color: rng.f64(),
            affine: Affine::random(rng),
            post: Affine::identity(),
            variations,
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

    /// A random genome with `n` transforms.
    pub fn random(rng: &mut Rng, n: usize) -> Genome {
        let transforms = (0..n).map(|_| Transform::random(rng)).collect();
        // Occasionally add a final transform for extra structure.
        let final_transform = if rng.chance(0.5) {
            Some(Transform::random(rng))
        } else {
            None
        };
        Genome {
            transforms,
            final_transform,
            palette: Palette::random(rng),
            camera: Camera::default(),
            brightness: 1.0,
            gamma: 2.2,
            vibrancy: 1.0,
            background: [0.0, 0.0, 0.0],
        }
    }
}
