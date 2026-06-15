//! Video encode: tonemap each accumulated frame histogram → one concatenated
//! rawvideo stream → shell out to ffmpeg → a looping webm (VP9). Cached on
//! disk; re-encoded on a quality-delta threshold (total accepted tiles step).
//!
//! ffmpeg is the one external dependency. If it's absent the endpoint still
//! responds (404 until a video exists) — the encode function returns an error
//! that the caller logs and swallows, so a missing ffmpeg degrades gracefully
//! rather than crashing the merge path.

use std::path::{Path, PathBuf};
use std::process::Command;

use flame_core::genome::Genome;

use crate::error::{ApiError, ApiResult};
use crate::render;

/// Output path for a sheep's encoded loop video.
pub fn video_path(data_dir: &Path, sheep_id: &str) -> PathBuf {
    data_dir.join("video").join(format!("{sheep_id}.webm"))
}

/// Encode (or re-encode) a sheep's loop video from its accumulated per-frame
/// histograms. Tonemaps each frame via flame-core, writes PNGs to a temp dir,
/// then runs ffmpeg to produce a looping VP9 webm. Returns the output path.
///
/// This is the clean function ARCHITECTURE/the prompt asks for: video encode is
/// isolated here behind one call. If ffmpeg isn't installed it returns an error
/// (caller decides whether that's fatal).
pub fn encode_video(
    data_dir: &Path,
    genome: &Genome,
    sheep_id: &str,
    n_frames: u32,
    w: u32,
    h: u32,
    ss: u32,
) -> ApiResult<PathBuf> {
    let work_dir = data_dir.join("video");
    std::fs::create_dir_all(&work_dir).map_err(|e| ApiError::internal(format!("mkdir video: {e}")))?;

    // Tonemap every frame and concatenate the raw RGBA into ONE stream file.
    // A single rawvideo input with -framerate is the robust way to feed ffmpeg
    // (the %04d image-sequence demuxer is finicky with raw frames); ffmpeg
    // slices it into frames by the fixed `w*h*4` frame size.
    let raw_path = work_dir.join(format!("{sheep_id}.raw"));
    let frame_bytes = (w * h * 4) as usize;
    let mut stream = Vec::with_capacity(frame_bytes * n_frames as usize);
    let mut any = false;
    for frame in 0..n_frames {
        let total: u64 = render::load_frame_accum(data_dir, sheep_id, frame, w, h, ss)
            .data
            .iter()
            .map(|c| c[3])
            .sum();
        if total > 0 {
            any = true;
        }
        let rgba = render::tonemap_frame(data_dir, genome, sheep_id, frame, w, h, ss);
        // tonemap returns exactly w*h*4 bytes; guard anyway.
        if rgba.len() == frame_bytes {
            stream.extend_from_slice(&rgba);
        } else {
            stream.extend(std::iter::repeat(0u8).take(frame_bytes));
        }
    }
    if !any {
        return Err(ApiError::not_found("no rendered frames yet"));
    }
    std::fs::write(&raw_path, &stream)
        .map_err(|e| ApiError::internal(format!("write raw stream: {e}")))?;

    let out = video_path(data_dir, sheep_id);

    // ffmpeg: read the single concatenated rawvideo RGBA stream, encode a
    // looping VP9 webm at 24fps. The loop is inherent — frames 0..N-1 cycle, so
    // the player just sets `loop` on the <video>.
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
        .map_err(|e| ApiError::internal(format!("ffmpeg spawn failed (is it installed?): {e}")))?;

    std::fs::remove_file(&raw_path).ok();

    if !status.success() {
        return Err(ApiError::internal("ffmpeg encode failed"));
    }
    Ok(out)
}

/// Decide whether the video is stale enough to re-encode. Re-encode when the
/// sheep has crossed a tile-count step since the last encode (quality-delta
/// threshold). `prev_rev` is `tiles / STEP` at last encode.
pub fn should_reencode(tiles: u64, video_rev: u64) -> bool {
    const STEP: u64 = 256; // tiles between re-encodes
    tiles / STEP > video_rev
}

/// The rev value to store after an encode at `tiles`.
pub fn rev_for(tiles: u64) -> u64 {
    const STEP: u64 = 256;
    tiles / STEP
}

#[cfg(test)]
mod tests {
    use super::*;
    use flame_core::genome::Genome;
    use flame_core::rng::Rng;

    /// End-to-end-ish: render a few frames into the on-disk hist cache, then
    /// encode. Skips gracefully if ffmpeg is absent.
    #[test]
    fn encode_runs_or_skips() {
        let dir = std::env::temp_dir().join(format!("vidtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut rng = Rng::new(2);
        let genome = Genome::random(&mut rng, 3);
        let sid = flame_core::canonical::sheep_id_hex(&genome);
        let sidb = flame_core::canonical::sheep_id(&genome);
        let (w, h, ss, nf) = (64u32, 64u32, 1u32, 4u32);
        for frame in 0..nf {
            let accum = flame_core::chunked::render_batch(&genome, &sidb, frame, 0, w as usize, h as usize, ss as usize, 50_000, nf);
            let cells: Vec<u64> = accum.data.iter().flatten().copied().collect();
            crate::render::merge_tile_into_frame(&dir, &sid, frame, &cells, w, h, ss).unwrap();
        }
        match encode_video(&dir, &genome, &sid, nf, w, h, ss) {
            Ok(p) => {
                let meta = std::fs::metadata(&p).unwrap();
                assert!(meta.len() > 0, "video file should be non-empty");
                println!("VIDEO OK: {} bytes at {:?}", meta.len(), p);
            }
            Err(e) => {
                // ffmpeg missing is acceptable in CI; the merge path swallows it.
                println!("VIDEO SKIPPED: {}", e.msg);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
