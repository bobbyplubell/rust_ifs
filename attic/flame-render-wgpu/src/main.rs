//! `flame-gpu` — render a genome on the GPU (wgpu) to a PNG.
//!
//!   flame-gpu --seed 7 --out gpu.png [--width W --height H --ss S --samples M]
//!   flame-gpu --in genome.json --out gpu.png [...]

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use flame_core::genome::Genome;
use flame_render::{render_frames_compute, render_gpu, render_gpu_compute, GpuOpts};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut opts: HashMap<String, String> = HashMap::new();
    while let Some(k) = args.next() {
        if let Some(key) = k.strip_prefix("--") {
            opts.insert(key.to_string(), args.next().unwrap_or_else(|| "1".into()));
        }
    }

    let gpu = GpuOpts {
        width: get(&opts, "width", 800),
        height: get(&opts, "height", 800),
        ss: get(&opts, "ss", 2),
        samples: get(&opts, "samples", 8_000_000),
        burn_in: get(&opts, "burn-in", 20),
        seed: get(&opts, "seed", 0),
    };

    let genome = if let Some(input) = opts.get("in") {
        let text = fs::read_to_string(input).expect("read genome json");
        serde_json::from_str(&text).expect("parse genome json")
    } else {
        let mut rng = flame_core::rng::Rng::new(gpu.seed);
        Genome::random(&mut rng, get(&opts, "transforms", 3))
    };

    let frames: usize = get(&opts, "frames", 1);
    let t0 = std::time::Instant::now();

    if frames > 1 {
        // Rotation loop, one GPU context for all frames (for animation/breeding).
        let dir = opts.get("out-dir").cloned().unwrap_or_else(|| "frames".into());
        fs::create_dir_all(&dir).expect("create out-dir");
        let imgs = render_frames_compute(&genome, &gpu, frames);
        for (i, rgba) in imgs.iter().enumerate() {
            save_png(&format!("{dir}/frame_{i:04}.png"), rgba, gpu.width, gpu.height);
        }
        eprintln!("GPU rendered {frames} frames in {:.2}s -> {dir}/", t0.elapsed().as_secs_f64());
        return;
    }

    let out = opts.get("out").cloned().unwrap_or_else(|| "gpu.png".into());
    // "compute" = full-GPU chaos game (default); "points" = CPU chaos game +
    // GPU additive blend.
    let mode = opts.get("mode").map(String::as_str).unwrap_or("compute");
    let rgba = match mode {
        "points" => render_gpu(&genome, &gpu),
        _ => render_gpu_compute(&genome, &gpu),
    };
    eprintln!("GPU render ({mode}): {:.2}s", t0.elapsed().as_secs_f64());
    save_png(&out, &rgba, gpu.width, gpu.height);
    eprintln!("wrote {out} ({}x{})", gpu.width, gpu.height);
}

fn get<T: std::str::FromStr>(opts: &HashMap<String, String>, key: &str, default: T) -> T {
    opts.get(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn save_png(path: &str, rgba: &[u8], width: usize, height: usize) {
    let file = fs::File::create(Path::new(path)).expect("create png");
    let w = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(rgba).expect("png data");
}
