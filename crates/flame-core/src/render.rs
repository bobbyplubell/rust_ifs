//! The chaos game + Draves tone mapping.
//!
//! Pipeline:
//!   1. Iterate the IFS, plotting points into a high-res linear accumulation
//!      buffer (`r_sum, g_sum, b_sum, count` per cell).
//!   2. Box-downsample the accumulation by the supersample factor.
//!   3. Tone-map: average color = rgb_sum/count, brightness from log(count),
//!      then gamma + vibrancy. This is the step the original code had but left
//!      as dead (shadowed) code, which is why its output didn't look like a
//!      flame.
//!
//! The accumulation (`Accum`) and tone map are public and separable so the
//! chunked/progressive renderer (`crate::chunked`) can accumulate independent
//! chunks, merge them element-wise, and tone-map the running sum at any point.

use crate::fmath;
use crate::genome::Genome;
use crate::rng::Rng;

/// Quality / size knobs for a single render.
#[derive(Debug, Clone, Copy)]
pub struct RenderOpts {
    pub width: usize,
    pub height: usize,
    /// Linear supersample factor (accumulate at width*ss x height*ss).
    pub ss: usize,
    /// Number of plotted iterations.
    pub samples: u64,
    /// Iterations discarded before plotting (settle onto the attractor).
    pub burn_in: u64,
    /// PRNG seed — same seed + same genome => identical image, everywhere.
    pub seed: u64,
}

impl Default for RenderOpts {
    fn default() -> Self {
        RenderOpts {
            width: 800,
            height: 800,
            ss: 2,
            samples: 20_000_000,
            burn_in: 20,
            seed: 0,
        }
    }
}

/// High-res linear accumulation buffer (histogram): per cell
/// `[r_sum, g_sum, b_sum, count]`, row-major, `w` cells per row.
///
/// Histograms are additive: the protocol render is the element-wise sum of
/// independent chunk accumulations (see `crate::chunked`).
#[derive(Debug, Clone)]
pub struct Accum {
    pub w: usize,
    pub h: usize,
    pub data: Vec<[f64; 4]>,
}

impl Accum {
    pub fn new(w: usize, h: usize) -> Self {
        Accum {
            w,
            h,
            data: vec![[0.0; 4]; w * h],
        }
    }

    #[inline]
    pub fn add(&mut self, x: usize, y: usize, rgb: [f64; 3]) {
        self.add_scaled(x, y, rgb, 1.0);
    }

    /// Add with an intensity scale (directional motion blur: earlier temporal
    /// steps contribute less; see Draves sec. 9.1).
    #[inline]
    pub fn add_scaled(&mut self, x: usize, y: usize, rgb: [f64; 3], s: f64) {
        let cell = &mut self.data[y * self.w + x];
        cell[0] += rgb[0] * s;
        cell[1] += rgb[1] * s;
        cell[2] += rgb[2] * s;
        cell[3] += s;
    }

    /// Element-wise add `other` into `self`. Panics on dimension mismatch.
    pub fn merge(&mut self, other: &Accum) {
        assert_eq!(self.w, other.w, "accum width mismatch");
        assert_eq!(self.h, other.h, "accum height mismatch");
        for (a, b) in self.data.iter_mut().zip(other.data.iter()) {
            a[0] += b[0];
            a[1] += b[1];
            a[2] += b[2];
            a[3] += b[3];
        }
    }
}

/// Run the chaos game, invoking `plot(world_x, world_y, linear_rgb)` for each
/// plotted point (after burn-in, in world coordinates with its palette color).
///
/// This is the shared core: `render` accumulates these into a histogram; other
/// consumers (e.g. a streaming/progressive renderer) can plot them their own way
/// and still draw the *same* flame.
pub fn iterate(
    genome: &Genome,
    samples: u64,
    burn_in: u64,
    seed: u64,
    mut plot: impl FnMut(f64, f64, [f64; 3]),
) {
    let mut rng = Rng::new(seed);
    let mut x = rng.range(-1.0, 1.0);
    let mut y = rng.range(-1.0, 1.0);
    let mut color = rng.f64();
    let mut prev: Option<usize> = None;

    let total = samples + burn_in;
    for i in 0..total {
        let t = genome.pick(prev, &mut rng);
        prev = Some(t);
        genome.transforms[t].apply(&mut x, &mut y, &mut color, &mut rng);

        // Reseed if the trajectory escaped to infinity / NaN.
        if !x.is_finite() || !y.is_finite() {
            x = rng.range(-1.0, 1.0);
            y = rng.range(-1.0, 1.0);
            color = rng.f64();
            continue;
        }

        if i < burn_in {
            continue;
        }

        // Optional final transform affects only the plotted point, not the
        // ongoing trajectory.
        let (mut px, mut py, mut pc) = (x, y, color);
        if let Some(ft) = &genome.final_transform {
            ft.apply(&mut px, &mut py, &mut pc, &mut rng);
        }
        plot(px, py, genome.palette.color(pc));
    }
}

/// Run the chaos game for `(samples, burn_in, seed)` and accumulate the plotted
/// points into `accum` (which must be sized `width*ss x height*ss` for the
/// camera framing implied by its dimensions).
pub fn accumulate(genome: &Genome, samples: u64, burn_in: u64, seed: u64, accum: &mut Accum) {
    accumulate_scaled(genome, samples, burn_in, seed, accum, 1.0);
}

/// `accumulate` with an intensity scale on every plotted point (directional
/// motion blur, Draves sec. 9.1).
pub fn accumulate_scaled(
    genome: &Genome,
    samples: u64,
    burn_in: u64,
    seed: u64,
    accum: &mut Accum,
    scale: f64,
) {
    let (hw, hh) = (accum.w, accum.h);
    let to_img = genome.camera.world_to_image(hw, hh);

    iterate(genome, samples, burn_in, seed, |px, py, rgb| {
        let (ix, iy) = to_img.apply(px, py);
        if ix >= 0.0 && iy >= 0.0 && ix < hw as f64 && iy < hh as f64 {
            accum.add_scaled(ix as usize, iy as usize, rgb, scale);
        }
    });
}

/// Run the chaos game and tone-map to an RGBA8 image (`width*height*4` bytes).
pub fn render(genome: &Genome, opts: &RenderOpts) -> Vec<u8> {
    let hw = opts.width * opts.ss;
    let hh = opts.height * opts.ss;
    let mut accum = Accum::new(hw, hh);
    accumulate(genome, opts.samples, opts.burn_in, opts.seed, &mut accum);
    tonemap(&accum, genome, opts.width, opts.height, opts.ss)
}

/// Tone-map `accum` to RGBA8 (`width*height*4`) the flam3 way:
///
///   1. Resolve EVERY supersampled cell through the log-density map first
///      (exposure from a high percentile of nonzero densities). Fine bright
///      structure is preserved: a one-cell filament gets its log boost before
///      anything averages it.
///   2. Adaptive density estimation at supersample resolution: gather radius
///      shrinks as density grows (sparse glow smooths, dense edges stay
///      sharp) — the real flam3 kernel idea, not a fixed output-res blur.
///   3. Gaussian-weighted downsample of the *resolved* colors (filtering in
///      log space, like flam3's spatial filter).
///   4. Per-channel gamma with a linear threshold near zero (gamma 4 on
///      near-empty cells amplifies speckle; the linear toe suppresses it),
///      vibrancy blend, background composite.
///
/// Pure function of the accumulation buffer — display-side only; proofs hash
/// the raw accumulation and never see any of this.
pub fn tonemap(accum: &Accum, genome: &Genome, width: usize, height: usize, ss: usize) -> Vec<u8> {
    assert_eq!(accum.w, width * ss, "accum width != width*ss");
    assert_eq!(accum.h, height * ss, "accum height != height*ss");
    let (ow, oh) = (width, height);
    let (hw, hh) = (accum.w, accum.h);

    // Exposure: normalize log-density by the 99.5th percentile of nonzero
    // supersampled-cell counts (strict max lets one hot cell dim everything).
    let (norm_count, mean_nz) = {
        let mut counts: Vec<f64> = accum.data.iter().map(|c| c[3]).filter(|&c| c > 0.0).collect();
        if counts.is_empty() {
            (0.0, 0.0)
        } else {
            let mean = counts.iter().sum::<f64>() / counts.len() as f64;
            counts.sort_unstable_by(f64::total_cmp);
            (counts[((counts.len() - 1) as f64 * 0.995) as usize], mean)
        }
    };
    let log_max = fmath::log(1.0 + norm_count).max(1e-12);
    let brightness = genome.brightness;
    let bg = genome.background;

    // 1. Resolve each supersampled cell: linear color scaled by log-density
    // luminance l in [0,1]; alpha channel carries l for compositing.
    let mut resolved = vec![[0.0f64; 4]; hw * hh];
    for (i, cell) in accum.data.iter().enumerate() {
        let c = cell[3];
        if c <= 0.0 {
            continue;
        }
        let l = (fmath::log(1.0 + c) / log_max * brightness).clamp(0.0, 1.0);
        let s = l / c;
        resolved[i] = [cell[0] * s, cell[1] * s, cell[2] * s, l];
    }

    // 2. Adaptive density estimation at supersample resolution. Radius per
    // cell from its own count relative to the image mean: empty/sparse cells
    // gather wide (glow), dense cells gather narrow (detail). Weights are
    // 1/(d2+1) — close enough to gaussian, exact and cheap.
    let de = if mean_nz > 0.0 {
        let mut out = resolved.clone();
        for y in 0..hh {
            for x in 0..hw {
                let c = accum.data[y * hw + x][3];
                let rel = c / mean_nz;
                let rad: i64 = if rel >= 1.0 {
                    0
                } else if rel >= 0.25 {
                    1
                } else if rel >= 0.05 {
                    2
                } else {
                    3
                };
                if rad == 0 {
                    continue;
                }
                let mut acc = [0.0f64; 4];
                let mut wsum = 0.0;
                for dy in -rad..=rad {
                    for dx in -rad..=rad {
                        let nx = x as i64 + dx;
                        let ny = y as i64 + dy;
                        if nx < 0 || ny < 0 || nx >= hw as i64 || ny >= hh as i64 {
                            continue;
                        }
                        let w = 1.0 / ((dx * dx + dy * dy) as f64 + 1.0);
                        let v = &resolved[ny as usize * hw + nx as usize];
                        for k in 0..4 {
                            acc[k] += w * v[k];
                        }
                        wsum += w;
                    }
                }
                let o = &mut out[y * hw + x];
                for k in 0..4 {
                    o[k] = acc[k] / wsum;
                }
            }
        }
        out
    } else {
        resolved
    };

    // 3. Gaussian-weighted downsample of resolved (log-space) colors, with a
    // one-cell apron so the filter spans block boundaries.
    let inv_gamma = 1.0 / genome.gamma;
    let vib = genome.vibrancy.clamp(0.0, 1.0);
    // Linear toe: below this luminance, gamma is applied as a linear ramp
    // anchored at the threshold (flam3's gamma_lin_thresh idea).
    const LIN_THRESH: f64 = 0.005;
    let thresh_gamma = fmath::pow(LIN_THRESH, inv_gamma) / LIN_THRESH;

    let sigma = 0.45 * ss as f64;
    let two_sigma2 = 2.0 * sigma * sigma;
    let half = (ss as f64 - 1.0) * 0.5;
    let mut out = vec![0u8; ow * oh * 4];
    for oy in 0..oh {
        for ox in 0..ow {
            let mut acc = [0.0f64; 4];
            let mut wsum = 0.0;
            let apron = 1i64;
            for dy in -apron..ss as i64 + apron {
                for dx in -apron..ss as i64 + apron {
                    let sx = ox as i64 * ss as i64 + dx;
                    let sy = oy as i64 * ss as i64 + dy;
                    if sx < 0 || sy < 0 || sx >= hw as i64 || sy >= hh as i64 {
                        continue;
                    }
                    let fx = dx as f64 - half;
                    let fy = dy as f64 - half;
                    let w = fmath::exp(-(fx * fx + fy * fy) / two_sigma2);
                    let v = &de[sy as usize * hw + sx as usize];
                    for k in 0..4 {
                        acc[k] += w * v[k];
                    }
                    wsum += w;
                }
            }
            let l = (acc[3] / wsum).clamp(0.0, 1.0);
            let i = oy * ow + ox;
            if l <= 0.0 {
                out[i * 4] = to_u8(bg[0]);
                out[i * 4 + 1] = to_u8(bg[1]);
                out[i * 4 + 2] = to_u8(bg[2]);
                out[i * 4 + 3] = 255;
                continue;
            }
            // Gamma factor from the filtered luminance (with linear toe), then
            // vibrancy blend between luma-gamma and per-channel gamma.
            let g_luma = if l < LIN_THRESH {
                thresh_gamma
            } else {
                fmath::pow(l, inv_gamma) / l
            };
            let mut ch = [0.0f64; 3];
            for k in 0..3 {
                let lin = (acc[k] / wsum).max(0.0);
                let by_brightness = lin * g_luma;
                let by_channel = if lin < LIN_THRESH {
                    lin * thresh_gamma
                } else {
                    fmath::pow(lin, inv_gamma)
                };
                let col = vib * by_brightness + (1.0 - vib) * by_channel;
                ch[k] = col + bg[k] * (1.0 - l);
            }
            out[i * 4] = to_u8(ch[0]);
            out[i * 4 + 1] = to_u8(ch[1]);
            out[i * 4 + 2] = to_u8(ch[2]);
            out[i * 4 + 3] = 255;
        }
    }
    out
}

#[inline]
fn to_u8(v: f64) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}
