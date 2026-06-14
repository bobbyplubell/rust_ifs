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
//!   sheep-id      genome JSON -> sha-256 of its canonical form
//!   breed         deterministically breed two genome JSONs
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
        "sheep-id" => cmd_sheep_id(&opts),
        "breed" => cmd_breed(&opts),
        "chunk-hashes" => cmd_chunk_hashes(&opts),
        "challenge" => {
            // Print challenge_from_seed(--seed) as hex — the per-sheep challenge
            // that chunk-hashes / determinism.html use.
            let seed: u64 = get(&opts, "seed", 0);
            println!("{}", flame_core::chunked::to_hex(&flame_core::chunked::challenge_from_seed(seed)));
        }
        "frame-hashes" => cmd_frame_hashes(&opts),
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
         \x20 sheep-id     --in g.json   (prints the sha-256 of the canonical genome json)\n\
         \x20 breed        --in-a a.json --in-b b.json --challenge <64-hex> [--out child.json]\n\
         \x20 chunk-hashes --in g.json --challenge <hex> --chunks N --samples-per-chunk N --width W --height H --ss S\n\
         \x20 frame-hashes --in g.json --challenge <hex> --frames N --samples-per-frame N --temporal T --width W --height H --ss S   (loop proof)\n"
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
    let frames: usize = get(opts, "frames", 60);
    let rotate = opts.contains_key("rotate");
    let dir = opts.get("out-dir").cloned().unwrap_or_else(|| "frames".into());
    fs::create_dir_all(&dir).expect("create out-dir");

    // Keyframes: --seeds a,b,c,... (flam3-style smooth Catmull-Rom loop), or
    // the legacy --seed-a/--seed-b pair (a 2-key cycle through the spline).
    let seeds: Vec<u64> = match opts.get("seeds") {
        Some(s) => s.split(',').map(|v| v.trim().parse().expect("bad --seeds")).collect(),
        None => vec![get(opts, "seed-a", 1), get(opts, "seed-b", 2)],
    };
    assert!(seeds.len() >= 2, "need at least 2 keyframe seeds");
    let mut keys: Vec<Genome> = seeds
        .iter()
        .map(|&s| {
            let mut rng = flame_core::rng::Rng::new(s);
            Genome::random(&mut rng, transforms)
        })
        .collect();
    // Matching final-transform presence interpolates cleanly.
    if keys.iter().any(|g| g.final_transform.is_none()) {
        for g in &mut keys {
            g.final_transform = None;
        }
    }

    for f in 0..frames {
        let phase = f as f64 / frames as f64;
        let mut g = flame_core::interpolate::spline_loop(&keys, phase);
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
    let mut genome: Genome = serde_json::from_str(&text).expect("parse genome json");
    // Optional flam3-style animation phase (0..1): rotated bases + palette drift.
    let phase: f64 = get(opts, "phase", 0.0);
    if phase != 0.0 {
        genome = flame_core::animate::animated(&genome, phase);
    }
    let out = opts.get("out").cloned().unwrap_or_else(|| "flame.png".into());
    let rgba = render(&genome, &ropts);
    save_png(&out, &rgba, ropts.width, ropts.height);
    eprintln!("wrote {out} from {input}");
}

fn load_genome(path: &str, what: &str) -> Genome {
    let text = fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("cannot read {what} ({path}): {e}");
        exit(1);
    });
    let genome: Genome = serde_json::from_str(&text).unwrap_or_else(|e| {
        eprintln!("bad genome json in {path}: {e}");
        exit(1);
    });
    if let Err(e) = genome.validate() {
        eprintln!("invalid genome in {path}: {e}");
        exit(1);
    }
    genome
}

fn decode_challenge(hex: &str) -> [u8; 32] {
    let hex = hex.trim();
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        eprintln!("--challenge must be 64 hex chars (32 bytes)");
        exit(1);
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("checked hex");
    }
    bytes
}

fn cmd_sheep_id(opts: &HashMap<String, String>) {
    let input = opts.get("in").expect("--in genome.json required");
    let genome = load_genome(input, "--in");
    println!("{}", flame_core::canonical::sheep_id_hex(&genome));
}

fn cmd_breed(opts: &HashMap<String, String>) {
    let in_a = opts.get("in-a").expect("--in-a a.json required");
    let in_b = opts.get("in-b").expect("--in-b b.json required");
    let challenge = opts.get("challenge").expect("--challenge <64-hex> required");
    let out = opts.get("out").cloned().unwrap_or_else(|| "child.json".into());

    let a = load_genome(in_a, "--in-a");
    let b = load_genome(in_b, "--in-b");
    let bytes = decode_challenge(challenge);
    let seed = u64::from_le_bytes(bytes[0..8].try_into().unwrap());

    let child = flame_core::breed::breed(&a, &b, seed);
    if let Err(e) = child.validate() {
        eprintln!("bred child failed validation: {e}");
        exit(1);
    }
    let json = flame_core::canonical::canonical_json(&child);
    fs::write(&out, &json).expect("write child json");
    eprintln!("wrote {out}");
    println!("{}", flame_core::canonical::sheep_id_hex(&child));
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

// Native side of a protocol-v3 loop-proof audit: per-frame hashes.
fn cmd_frame_hashes(opts: &HashMap<String, String>) {
    let input = opts.get("in").expect("--in genome.json required");
    let challenge_hex = opts.get("challenge").expect("--challenge <hex> required");
    let challenge = flame_core::chunked::challenge_from_hex(challenge_hex)
        .unwrap_or_else(|e| panic!("bad --challenge: {e}"));
    let frames: u32 = get(opts, "frames", 64);
    let samples_per_frame: u64 = get(opts, "samples-per-frame", 240_000);
    let temporal: u32 = get(opts, "temporal", 2);
    let width: usize = get(opts, "width", 256);
    let height: usize = get(opts, "height", 256);
    let ss: usize = get(opts, "ss", 1);

    let genome = load_genome(input, "--in");
    for idx in 0..frames {
        let accum = flame_core::chunked::render_proof_frame(
            &genome, width, height, ss, samples_per_frame, &challenge, idx, frames, temporal,
        );
        println!("{idx}: {}", flame_core::chunked::chunk_hash_hex(&accum));
    }
}
