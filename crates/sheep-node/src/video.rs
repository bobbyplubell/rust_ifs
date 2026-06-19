//! Tonemap → ffmpeg loop-video encode for the watch face (§1.1 accumulate →
//! tonemap → video; §5 video is the served artifact).
//!
//! This is the **ported minimal tonemap→ffmpeg path** from `coordinator/src/
//! video.rs`. The coordinator's `encode_video` reads each frame's histogram from
//! its on-disk hist cache (`render::load_frame_accum` / `tonemap_frame`); the v3
//! node already holds the merged per-`(sheep, frame)` histograms in-memory in the
//! [`Accumulator`] (the CRDT), so we tonemap straight off `Accumulator::tonemap`
//! instead. The ffmpeg invocation itself (single concatenated rawvideo RGBA
//! stream → looping VP9 webm at 24fps) is reproduced **verbatim** from the
//! coordinator — the encoder is not reinvented. The coordinator crate is not a
//! library dependency of this pure-engine crate, so factoring its function out
//! was more invasive than porting the ~20 lines of ffmpeg shell-out; we port and
//! flag it here per the brief.
//!
//! ffmpeg is the one external dependency. If it's absent the encode returns an
//! error string the HTTP layer turns into a 404 — a missing ffmpeg degrades
//! gracefully (the histograms + tonemap path still work, only the encode is
//! skipped). The video is a **disposable cache** (§5): regenerable from the
//! accumulation, so losing/skipping it is never fatal.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::accumulator::Accumulator;

/// Tiles between re-encodes (the quality-delta threshold, mirrors the
/// coordinator's `video::should_reencode` STEP).
const REENCODE_STEP: usize = 64;

/// Output path for a sheep's encoded loop video (mirrors coordinator layout).
pub fn video_path(data_dir: &Path, sheep_id: &str) -> PathBuf {
    data_dir.join("video").join(format!("{sheep_id}.webm"))
}

/// Sidecar recording the tile count at the last encode, so we only re-encode
/// when coverage advances materially (§ "regenerate when coverage advances").
fn rev_path(data_dir: &Path, sheep_id: &str) -> PathBuf {
    data_dir.join("video").join(format!("{sheep_id}.rev"))
}

/// Ensure a current loop video exists for `sheep_id`, encoding (or re-encoding)
/// from the accumulator's merged frames if it is missing or stale (coverage has
/// crossed a tile-count step since the cached encode). Returns the on-disk path
/// on success, or an error string (no frames yet / ffmpeg missing / encode
/// failure) the HTTP layer maps to 404. Cheap when the cache is current: only
/// reads the accumulator's tile count, no tonemap.
pub fn ensure_video(
    accum: &Mutex<Accumulator>,
    data_dir: &Path,
    sheep_id: &str,
    n_frames: u32,
) -> Result<PathBuf, String> {
    let out = video_path(data_dir, sheep_id);

    // Total live tiles across all frames = the coverage/density measure.
    let tiles: usize = {
        let a = accum.lock().unwrap();
        (0..n_frames).map(|f| a.tile_count(sheep_id, f)).sum()
    };
    if tiles == 0 {
        return Err("no accumulated frames yet for this sheep".into());
    }

    // Reuse the cached encode if it is current (coverage hasn't crossed a step).
    let last_rev = read_rev(data_dir, sheep_id);
    let cur_rev = tiles / REENCODE_STEP;
    if out.exists() && cur_rev <= last_rev {
        return Ok(out);
    }

    encode_video(accum, data_dir, sheep_id, n_frames)?;
    write_rev(data_dir, sheep_id, cur_rev);
    Ok(out)
}

/// Encode (or re-encode) a sheep's loop video from the accumulator's merged
/// per-frame histograms. Tonemaps each frame in-memory (no disk hist cache),
/// concatenates the raw RGBA into ONE rawvideo stream, then runs ffmpeg to a
/// looping VP9 webm. The ffmpeg invocation is reproduced verbatim from
/// `coordinator/src/video.rs::encode_video`.
fn encode_video(
    accum: &Mutex<Accumulator>,
    data_dir: &Path,
    sheep_id: &str,
    n_frames: u32,
) -> Result<PathBuf, String> {
    // Tonemap every frame off the in-memory accumulator and learn the edge from
    // the first non-empty frame (all of a sheep's tiles share its tier edge).
    let frames = tonemap_frames(accum, sheep_id, n_frames)?;
    let edge = frames.edge;
    let frame_bytes = edge * edge * 4;

    let work_dir = data_dir.join("video");
    std::fs::create_dir_all(&work_dir).map_err(|e| format!("mkdir video: {e}"))?;

    // One concatenated rawvideo file: missing frames are zero-filled so the
    // stream stays exactly `n_frames * frame_bytes` and the loop length holds.
    let raw_path = work_dir.join(format!("{sheep_id}.raw"));
    let mut stream = Vec::with_capacity(frame_bytes * n_frames as usize);
    for f in 0..n_frames {
        match frames.rgba.get(&f) {
            Some(rgba) if rgba.len() == frame_bytes => stream.extend_from_slice(rgba),
            _ => stream.extend(std::iter::repeat(0u8).take(frame_bytes)),
        }
    }
    std::fs::write(&raw_path, &stream).map_err(|e| format!("write raw stream: {e}"))?;

    let out = video_path(data_dir, sheep_id);
    let w = edge as u32;
    let h = edge as u32;

    // ffmpeg: read the single concatenated rawvideo RGBA stream, encode a
    // looping VP9 webm at 24fps (verbatim from coordinator/src/video.rs).
    let status = Command::new("ffmpeg")
        .arg("-y")
        .args(["-f", "rawvideo", "-pixel_format", "rgba"])
        .args(["-video_size", &format!("{w}x{h}")])
        .args(["-framerate", "24"])
        .arg("-i")
        .arg(&raw_path)
        .args(["-c:v", "libvpx-vp9", "-b:v", "0", "-crf", "32"])
        .args(["-pix_fmt", "yuv420p"])
        .arg(&out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("ffmpeg spawn failed (is it installed?): {e}"))?;

    std::fs::remove_file(&raw_path).ok();

    if !status.success() {
        return Err("ffmpeg encode failed".into());
    }
    Ok(out)
}

/// The tonemapped frames for a sheep: a map `frame -> RGBA8` plus the edge.
/// Separated out so the encode and a headless test can both drive it without
/// depending on ffmpeg (the brief: verify the tonemap/serialization path even
/// when the encoder is flaky in CI).
pub struct Frames {
    pub edge: usize,
    pub rgba: std::collections::HashMap<u32, Vec<u8>>,
}

/// Tonemap every non-empty merged frame of `sheep_id` to RGBA8. Errors if the
/// sheep has no accumulated frames (or its genome was never registered, so
/// tonemap can't resolve the palette). This is the pure tonemap/serialization
/// step the test asserts independently of ffmpeg.
pub fn tonemap_frames(
    accum: &Mutex<Accumulator>,
    sheep_id: &str,
    n_frames: u32,
) -> Result<Frames, String> {
    let mut a = accum.lock().unwrap();
    let mut rgba = std::collections::HashMap::new();
    let mut edge = 0usize;
    for f in 0..n_frames {
        if let Some(img) = a.tonemap(sheep_id, f) {
            // edge*edge*4 == img.len() → edge = sqrt(len/4).
            let px = img.len() / 4;
            let e = (px as f64).sqrt() as usize;
            if e * e * 4 == img.len() {
                edge = e;
                rgba.insert(f, img);
            }
        }
    }
    if rgba.is_empty() {
        return Err("no tonemappable frames (no density or genome unregistered)".into());
    }
    Ok(Frames { edge, rgba })
}

fn read_rev(data_dir: &Path, sheep_id: &str) -> usize {
    std::fs::read_to_string(rev_path(data_dir, sheep_id))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn write_rev(data_dir: &Path, sheep_id: &str, rev: usize) {
    let _ = std::fs::write(rev_path(data_dir, sheep_id), rev.to_string());
}
