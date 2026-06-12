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
        let cell = &mut self.data[y * self.w + x];
        cell[0] += rgb[0];
        cell[1] += rgb[1];
        cell[2] += rgb[2];
        cell[3] += 1.0;
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

    let total = samples + burn_in;
    for i in 0..total {
        let t = genome.pick(&mut rng);
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
    let (hw, hh) = (accum.w, accum.h);
    let to_img = genome.camera.world_to_image(hw, hh);

    iterate(genome, samples, burn_in, seed, |px, py, rgb| {
        let (ix, iy) = to_img.apply(px, py);
        if ix >= 0.0 && iy >= 0.0 && ix < hw as f64 && iy < hh as f64 {
            accum.add(ix as usize, iy as usize, rgb);
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

/// Box-downsample `accum` by `ss`, then tone-map to RGBA8 (`width*height*4`).
///
/// `accum` must be `width*ss x height*ss`. Pure function of the accumulation
/// state, so it can be re-run on a running sum at any point for progressive
/// display.
pub fn tonemap(accum: &Accum, genome: &Genome, width: usize, height: usize, ss: usize) -> Vec<u8> {
    assert_eq!(accum.w, width * ss, "accum width != width*ss");
    assert_eq!(accum.h, height * ss, "accum height != height*ss");
    let (ow, oh) = (width, height);

    // 1. Downsample: sum each ss x ss block.
    let mut down = vec![[0.0f64; 4]; ow * oh];
    for oy in 0..oh {
        for ox in 0..ow {
            let mut acc = [0.0f64; 4];
            for dy in 0..ss {
                for dx in 0..ss {
                    let sx = ox * ss + dx;
                    let sy = oy * ss + dy;
                    let c = &accum.data[sy * accum.w + sx];
                    acc[0] += c[0];
                    acc[1] += c[1];
                    acc[2] += c[2];
                    acc[3] += c[3];
                }
            }
            down[oy * ow + ox] = acc;
        }
    }

    // 2. Max density for log normalization.
    let max_count = down.iter().map(|c| c[3]).fold(0.0f64, f64::max);
    let log_max = fmath::log(1.0 + max_count).max(1e-12);
    let inv_gamma = 1.0 / genome.gamma;
    let vib = genome.vibrancy.clamp(0.0, 1.0);

    // 3. Tone map.
    let mut out = vec![0u8; ow * oh * 4];
    for (i, cell) in down.iter().enumerate() {
        let freq = cell[3];
        let bg = genome.background;
        let (r, g, b);
        if freq <= 0.0 {
            r = bg[0];
            g = bg[1];
            b = bg[2];
        } else {
            // Average linear color in [0, 1].
            let avg = [cell[0] / freq, cell[1] / freq, cell[2] / freq];
            // Log-density brightness in [0, 1].
            let l = (fmath::log(1.0 + freq) / log_max * genome.brightness).clamp(0.0, 1.0);
            let lg = fmath::pow(l, inv_gamma);
            // Per-channel gamma vs gamma-on-brightness, blended by vibrancy.
            let mut ch = [0.0f64; 3];
            for c in 0..3 {
                let by_brightness = avg[c] * lg;
                let by_channel = fmath::pow(avg[c] * l, inv_gamma);
                let col = vib * by_brightness + (1.0 - vib) * by_channel;
                // Composite over background using coverage `l`.
                ch[c] = col + bg[c] * (1.0 - l);
            }
            r = ch[0];
            g = ch[1];
            b = ch[2];
        }
        out[i * 4] = to_u8(r);
        out[i * 4 + 1] = to_u8(g);
        out[i * 4 + 2] = to_u8(b);
        out[i * 4 + 3] = 255;
    }
    out
}

#[inline]
fn to_u8(v: f64) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}
