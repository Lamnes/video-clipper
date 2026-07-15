use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::debug;

pub fn check_ffmpeg() -> Result<()> {
    Command::new("ffmpeg").arg("-version").output().context("ffmpeg not found")?;
    Command::new("ffprobe").arg("-version").output().context("ffprobe not found")?;
    Ok(())
}

/// Extract audio as mp3 16kHz mono (small size, good for STT)
pub fn extract_audio(video: &Path, out: &Path) -> Result<()> {
    let s = Command::new("ffmpeg")
        .args(["-i", &video.to_string_lossy(),
            "-vn", "-acodec", "libmp3lame", "-ar", "16000", "-ac", "1", "-b:a", "64k",
            "-y", &out.to_string_lossy()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status().context("ffmpeg failed")?;
    if !s.success() { return Err(anyhow!("ffmpeg audio extraction failed")); }
    Ok(())
}

pub fn get_duration(path: &Path) -> Result<f64> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration",
            "-of", "default=noprint_wrappers=1:nokey=1", &path.to_string_lossy()])
        .output().context("ffprobe failed")?;
    if !out.status.success() { return Err(anyhow!("ffprobe error")); }
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().context("parse duration")
}

/// Split audio into chunks, returns Vec<(chunk_path, offset_seconds)>
pub fn split_audio_chunks(audio: &Path, chunk_secs: u32, dir: &Path) -> Result<Vec<(PathBuf, f64)>> {
    let total = get_duration(audio)?;
    let mut chunks = Vec::new();
    let mut offset = 0.0f64;
    let mut idx = 0u32;

    while offset < total {
        let p = dir.join(format!("chunk_{:04}.mp3", idx));
        // -ss BEFORE -i seeks via the container index (near-instant) instead of
        // decoding from the start of the file for every chunk.
        let s = Command::new("ffmpeg")
            .args(["-ss", &format!("{:.3}", offset),
                "-i", &audio.to_string_lossy(),
                "-t", &chunk_secs.to_string(),
                "-acodec", "libmp3lame", "-ar", "16000", "-ac", "1", "-b:a", "64k",
                "-y", &p.to_string_lossy()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if s.success() && p.exists() {
            let sz = std::fs::metadata(&p)?.len();
            if sz > 1024 {
                debug!("Chunk {}: offset={:.0}s size={:.1}KB", idx, offset, sz as f64 / 1024.0);
                chunks.push((p, offset));
            }
        }
        offset += chunk_secs as f64;
        idx += 1;
    }
    Ok(chunks)
}

/// Cut a segment from video with optional 9:16 vertical crop for Shorts/Reels.
/// Returns actual duration of the output file.
pub fn cut_segment(video: &Path, out: &Path, start: f64, end: f64, vertical_crop: bool) -> Result<f64> {
    let dur = end - start;

    // Accurate output seeking: -ss AFTER -i. This decodes from the previous
    // keyframe with references intact, so it's frame-accurate AND correct on any
    // source. Input seeking (-ss BEFORE -i) is faster but corrupts open-GOP H.264
    // (Blu-ray/broadcast remuxes): broken frame references produce a frozen
    // "picture + sound" clip. Correctness wins over the seek speed-up here.
    let mut args: Vec<String> = vec![
        "-i".into(), video.to_string_lossy().into_owned(),
        "-ss".into(), format!("{:.3}", start),
        "-t".into(), format!("{:.3}", dur),
    ];

    if vertical_crop {
        args.extend([
            "-vf".into(),
            "crop=ih*9/16:ih:(iw-ih*9/16)/2:0,scale=1080:1920:force_original_aspect_ratio=decrease,pad=1080:1920:(ow-iw)/2:(oh-ih)/2".into(),
        ]);
    }

    args.extend([
        "-c:v".into(), "libx264".into(),
        "-preset".into(), "fast".into(),
        "-crf".into(), "23".into(),
        "-c:a".into(), "aac".into(),
        "-b:a".into(), "128k".into(),
        "-avoid_negative_ts".into(), "make_zero".into(),
        "-y".into(), out.to_string_lossy().into_owned(),
    ]);

    let s = Command::new("ffmpeg")
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status().context("ffmpeg cut failed")?;
    if !s.success() { return Err(anyhow!("ffmpeg cut error for {:.1}-{:.1}s", start, end)); }

    // Verify actual output duration
    let actual_dur = get_duration(out).unwrap_or(0.0);
    Ok(actual_dur)
}
