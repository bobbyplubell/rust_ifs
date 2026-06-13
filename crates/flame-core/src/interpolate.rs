//! Genome interpolation — the basis of animated "sheep". A sheep is a smooth
//! path through genome space; here we provide the pairwise step A -> B that
//! callers stitch into loops.
//!
//! v1 does component-wise linear interpolation. It assumes both genomes have
//! the same number of transforms and the same final-transform presence, so
//! callers must only cross/animate compatible shapes. Affine
//! interpolation is naive lerp for now; a rotation-decomposed path can be
//! swapped into `Affine::lerp` later without touching callers.

use crate::genome::{Camera, Genome, Transform};

#[inline]
fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

impl Transform {
    pub fn lerp(&self, other: &Transform, t: f64) -> Transform {
        let variations = if self.variations.len() == other.variations.len() {
            self.variations
                .iter()
                .zip(other.variations.iter())
                .map(|(a, b)| lerp(*a, *b, t))
                .collect()
        } else {
            self.variations.clone()
        };
        let mut pvals = self.pvals;
        for (i, v) in pvals.iter_mut().enumerate() {
            *v = lerp(*v, other.pvals[i], t);
        }
        Transform {
            weight: lerp(self.weight, other.weight, t),
            color: lerp(self.color, other.color, t),
            affine: self.affine.lerp(&other.affine, t),
            post: self.post.lerp(&other.post, t),
            variations,
            pvals,
            color_speed: lerp(self.color_speed, other.color_speed, t),
        }
    }
}

impl Camera {
    pub fn lerp(&self, other: &Camera, t: f64) -> Camera {
        Camera {
            center_x: lerp(self.center_x, other.center_x, t),
            center_y: lerp(self.center_y, other.center_y, t),
            scale: lerp(self.scale, other.scale, t),
            rotate: lerp(self.rotate, other.rotate, t),
        }
    }
}

impl Genome {
    /// Interpolate toward `other` by `t` in [0, 1]. Requires matching transform
    /// counts; if they differ, returns a clone of `self` (the caller should
    /// only interpolate compatible genomes).
    pub fn lerp(&self, other: &Genome, t: f64) -> Genome {
        if self.transforms.len() != other.transforms.len() {
            return self.clone();
        }
        let transforms = self
            .transforms
            .iter()
            .zip(other.transforms.iter())
            .map(|(a, b)| a.lerp(b, t))
            .collect();

        let final_transform = match (&self.final_transform, &other.final_transform) {
            (Some(a), Some(b)) => Some(a.lerp(b, t)),
            (Some(a), None) => Some(a.clone()),
            (None, Some(b)) => Some(b.clone()),
            (None, None) => None,
        };

        Genome {
            transforms,
            final_transform,
            palette: self.palette.lerp(&other.palette, t),
            camera: self.camera.lerp(&other.camera, t),
            brightness: lerp(self.brightness, other.brightness, t),
            gamma: lerp(self.gamma, other.gamma, t),
            vibrancy: lerp(self.vibrancy, other.vibrancy, t),
            background: [
                lerp(self.background[0], other.background[0], t),
                lerp(self.background[1], other.background[1], t),
                lerp(self.background[2], other.background[2], t),
            ],
        }
    }
}
