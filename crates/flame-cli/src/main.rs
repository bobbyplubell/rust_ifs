//! `flame` — native CLI driver for the flame-core renderer.
//!
//! Subcommands:
//!   render        random genome from --seed -> PNG
//!   animate       interpolate two random genomes -> a folder of PNG frames
//!   dump          random genome from --seed -> genome JSON
//!   from-json     render a genome JSON -> PNG
//!   frames-json   render a genome JSON over a rotation loop -> PNG frames
//!   chunk-hashes  chunked protocol render of a genome JSON -> per-chunk +
//!                 final-RGBA SHA-256 hashes (native side of the browser
//!                 determinism check)
//!
//! Arg parsing is intentionally dependency-free `--key value`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::exit;

use flame_core::genome::Genome;
use flame_core::render::{render, RenderOpts};

fn main() {
    let mut args = std::env::args().skip(1);
    let cmd = match args.next() {
        Some(c) => c,
        None => {
            usage();
            exit(1);
        }
    };
    let opts = parse_kv(args);

    match cmd.as_str() {
        "render" => cmd_render(&opts),
        "animate" => cmd_animate(&opts),
        "dump" => cmd_dump(&opts),
        "from-json" => cmd_from_json(&opts),
        "frames-json" => cmd_frames_json(&opts),
        "chunk-hashes" => cmd_chunk_hashes(&opts),
        "-h" | "--help" | "help" => usage(),
        other => {
            eprintln!("unknown command: {other}\n");
            usage();
            exit(1);
        }
    }
}

fn usage() {
    eprintln!(
        "flame <command> [--key value ...]\n\
         \n\
         commands:\n\
         \x20 render       --seed N [--out f.png] [--width W --height H --ss S --samples M --transforms T]\n\
         \x20 animate      --seed-a A --seed-b B --frames N [--out-dir dir] [--rotate 1] [size/quality opts]\n\
         \x20 dump         --seed N [--out g.json] [--transforms T]\n\
         \x20 from-json    --in g.json [--out f.png] [--seed N] [size/quality opts]\n\
         \x20 frames-json  --in g.json --frames N [--out-dir dir] [size/quality opts]   (rotation loop)\n\
         \x20 chunk-hashes --in g.json --challenge <hex> --chunks N --samples-per-chunk N --width W --height H --ss S\n"
    );
}

// ---- shared option helpers -------------------------------------------------

fn parse_kv(args: impl Iterator<Item = String>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut args = args.peekable();
    while let Some(key) = args.next() {
        if let Some(stripped) = key.strip_prefix("--") {
            let val = args.next().unwrap_or_else(|| "1".to_string());
            map.insert(stripped.to_string(), val);
        }
    }
    map
}

fn get<T: std::str::FromStr>(opts: &HashMap<String, String>, key: &str, default: T) -> T {
    opts.get(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn render_opts(opts: &HashMap<String, String>) -> RenderOpts {
    let d = RenderOpts::default();
    RenderOpts {
        width: get(opts, "width", 800),
        height: get(opts, "height", 800),
        ss: get(opts, "ss", 2),
        samples: get(opts, "samples", d.samples),
        burn_in: get(opts, "burn-in", d.burn_in),
        seed: get(opts, "seed", 0),
    }
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

// ---- commands --------------------------------------------------------------

fn cmd_render(opts: &HashMap<String, String>) {
    let ropts = render_opts(opts);
    let transforms = get(opts, "transforms", 3usize);
    let mut rng = flame_core::rng::Rng::new(ropts.seed);
    let genome = Genome::random(&mut rng, transforms);
    let out = opts.get("out").cloned().unwrap_or_else(|| "flame.png".into());
    let rgba = render(&genome, &ropts);
    save_png(&out, &rgba, ropts.width, ropts.height);
    eprintln!("wrote {out} ({}x{}, seed {})", ropts.width, ropts.height, ropts.seed);
}

fn cmd_animate(opts: &HashMap<String, String>) {
    let ropts = render_opts(opts);
    let transforms = get(opts, "transforms", 3usize);
    let seed_a: u64 = get(opts, "seed-a", 1);
    let seed_b: u64 = get(opts, "seed-b", 2);
    let frames: usize = get(opts, "frames", 60);
    let rotate = opts.contains_key("rotate");
    let dir = opts.get("out-dir").cloned().unwrap_or_else(|| "frames".into());
    fs::create_dir_all(&dir).expect("create out-dir");

    // Two random genomes with the *same* shape so they interpolate cleanly.
    let mut rng_a = flame_core::rng::Rng::new(seed_a);
    let mut rng_b = flame_core::rng::Rng::new(seed_b);
    let mut a = Genome::random(&mut rng_a, transforms);
    let mut b = Genome::random(&mut rng_b, transforms);
    // Force matching final-transform presence for clean interpolation.
    if a.final_transform.is_some() != b.final_transform.is_some() {
        a.final_transform = None;
        b.final_transform = None;
    }

    for f in 0..frames {
        // Smooth there-and-back loop via a cosine ease over [0,1].
        let phase = f as f64 / frames as f64;
        let t = 0.5 - 0.5 * (phase * std::f64::consts::TAU).cos();
        let mut g = a.lerp(&b, t);
        if rotate {
            g.camera.rotate = phase * std::f64::consts::TAU;
        }
        let rgba = render(&g, &ropts);
        let path = format!("{dir}/frame_{f:04}.png");
        save_png(&path, &rgba, ropts.width, ropts.height);
        eprintln!("frame {}/{} -> {path}", f + 1, frames);
    }
    eprintln!("wrote {frames} frames to {dir}/");
}

fn cmd_dump(opts: &HashMap<String, String>) {
    let seed: u64 = get(opts, "seed", 0);
    let transforms = get(opts, "transforms", 3usize);
    let mut rng = flame_core::rng::Rng::new(seed);
    let genome = Genome::random(&mut rng, transforms);
    let json = serde_json::to_string_pretty(&genome).expect("serialize genome");
    match opts.get("out") {
        Some(path) => {
            fs::write(path, json).expect("write json");
            eprintln!("wrote genome json -> {path}");
        }
        None => println!("{json}"),
    }
}

fn cmd_from_json(opts: &HashMap<String, String>) {
    let ropts = render_opts(opts);
    let input = opts.get("in").expect("--in genome.json required");
    let text = fs::read_to_string(input).expect("read genome json");
    let genome: Genome = serde_json::from_str(&text).expect("parse genome json");
    let out = opts.get("out").cloned().unwrap_or_else(|| "flame.png".into());
    let rgba = render(&genome, &ropts);
    save_png(&out, &rgba, ropts.width, ropts.height);
    eprintln!("wrote {out} from {input}");
}

fn cmd_frames_json(opts: &HashMap<String, String>) {
    let ropts = render_opts(opts);
    let input = opts.get("in").expect("--in genome.json required");
    let frames: usize = get(opts, "frames", 16);
    let dir = opts.get("out-dir").cloned().unwrap_or_else(|| "frames".into());
    fs::create_dir_all(&dir).expect("create out-dir");
    let text = fs::read_to_string(input).expect("read genome json");
    let genome: Genome = serde_json::from_str(&text).expect("parse genome json");

    for f in 0..frames {
        let mut g = genome.clone();
        g.camera.rotate += (f as f64 / frames as f64) * std::f64::consts::TAU;
        let rgba = render(&g, &ropts);
        let path = format!("{dir}/frame_{f:04}.png");
        save_png(&path, &rgba, ropts.width, ropts.height);
    }
    eprintln!("wrote {frames} rotation frames to {dir}/ from {input}");
}

fn cmd_chunk_hashes(opts: &HashMap<String, String>) {
    let input = opts.get("in").expect("--in genome.json required");
    let challenge_hex = opts.get("challenge").expect("--challenge <hex> required");
    let challenge = flame_core::chunked::challenge_from_hex(challenge_hex)
        .unwrap_or_else(|e| panic!("bad --challenge: {e}"));
    let chunks: u32 = get(opts, "chunks", 8);
    let samples_per_chunk: u64 = get(opts, "samples-per-chunk", 50_000);
    let width: usize = get(opts, "width", 64);
    let height: usize = get(opts, "height", 64);
    let ss: usize = get(opts, "ss", 1);

    let text = fs::read_to_string(input).expect("read genome json");
    let genome: Genome = serde_json::from_str(&text).expect("parse genome json");

    let mut running = flame_core::render::Accum::new(width * ss, height * ss);
    for idx in 0..chunks {
        let chunk = flame_core::chunked::render_chunk(
            &genome,
            width,
            height,
            ss,
            samples_per_chunk,
            &challenge,
            idx,
        );
        println!("{idx}: {}", flame_core::chunked::chunk_hash_hex(&chunk));
        running.merge(&chunk);
    }
    let rgba = flame_core::render::tonemap(&running, &genome, width, height, ss);
    println!("rgba: {}", flame_core::chunked::sha256_hex(&rgba));
}
