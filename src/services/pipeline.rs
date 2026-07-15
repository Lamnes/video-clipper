use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing::error;
use uuid::Uuid;

use crate::db;
use crate::models::{ClipInfo, Highlight, JobStatus};
use crate::services::{analyzer, ffmpeg, transcribe};
use crate::state::AppState;

pub async fn run_pipeline(state: AppState, job_id: Uuid, video_path: PathBuf) {
    if let Err(e) = run_inner(&state, &job_id, &video_path).await {
        error!("[{}] Pipeline failed: {:?}", job_id, e);
        let _ = db::update_job_error(&state.db, &job_id, &e.to_string());
        state.send_progress(&job_id, "failed", 0, &e.to_string());
    }

    // Always delete the source video after processing to free disk space
    if video_path.exists() {
        let size = std::fs::metadata(&video_path).map(|m| m.len()).unwrap_or(0);
        match std::fs::remove_file(&video_path) {
            Ok(()) => tracing::info!("[{}] Deleted source video ({:.1} MB)", job_id, size as f64 / 1e6),
            Err(e) => tracing::warn!("[{}] Failed to delete source video: {}", job_id, e),
        }
    }

    state.cleanup_progress(&job_id);
}

async fn run_inner(state: &AppState, job_id: &Uuid, video_path: &PathBuf) -> Result<()> {
    let work_dir = state.job_dir(job_id);
    let clips_dir = state.clips_dir(job_id);
    std::fs::create_dir_all(&work_dir)?;
    std::fs::create_dir_all(&clips_dir)?;

    let job = db::get_job(&state.db, job_id)?.ok_or_else(|| anyhow::anyhow!("Job not found"))?;

    // 1. Extract audio
    state.send_progress(job_id, "extracting_audio", 5, "Extracting audio track");
    let audio_path = work_dir.join("audio.mp3");
    let vp = video_path.clone(); let ap = audio_path.clone();
    tokio::task::spawn_blocking(move || ffmpeg::extract_audio(&vp, &ap)).await?.context("extract audio")?;

    let vp2 = video_path.clone();
    let video_duration = tokio::task::spawn_blocking(move || ffmpeg::get_duration(&vp2)).await??;
    let _ = db::update_job_duration(&state.db, job_id, video_duration);
    state.send_progress(job_id, "extracting_audio", 15, &format!("Audio extracted ({:.0}s)", video_duration));

    // 2. Transcribe
    state.send_progress(job_id, "transcribing", 20, "Transcribing audio");
    let transcript = transcribe::transcribe_audio(
        &audio_path, &work_dir, &state.config, &job.language, &state.http_client,
    ).await.context("transcription")?;
    let _ = std::fs::remove_file(&audio_path);

    if transcript.segments.is_empty() {
        let _ = db::update_job_status(&state.db, job_id, &JobStatus::Completed, 100);
        state.send_progress(job_id, "completed", 100, "No speech detected");
        return Ok(());
    }
    std::fs::write(work_dir.join("transcript.json"), serde_json::to_string_pretty(&transcript)?)?;
    state.send_progress(job_id, "transcribing", 50, &format!("{} segments", transcript.segments.len()));

    // 3. Analyze + review
    state.send_progress(job_id, "analyzing", 55, "Analyzing for highlights");
    let mut highlights = analyzer::analyze_transcript(
        &transcript, video_duration, job.max_clips, job.min_clips, job.min_clip_duration, job.max_clip_duration,
        &state.config, &state.http_client, state, job_id,
    ).await.context("analysis")?;
    state.send_progress(job_id, "analyzing", 70, &format!("{} raw highlights from LLM", highlights.len()));

    if highlights.is_empty() {
        let _ = db::update_job_status(&state.db, job_id, &JobStatus::Completed, 100);
        state.send_progress(job_id, "completed", 100, "No highlights found");
        return Ok(());
    }

    // 4. Hard-enforce min/max duration (final safety net regardless of what LLM returned)
    let min_d = job.min_clip_duration as f64;
    let max_d = job.max_clip_duration as f64;
    let pad = state.config.clip_padding.max(0.0);
    let vd = video_duration;
    let before_count = highlights.len();

    // Breathing room so clips don't begin/end on an abrupt cut.
    if pad > 0.0 {
        for h in &mut highlights {
            h.start_time = (h.start_time - pad).max(0.0);
            h.end_time = (h.end_time + pad).min(vd);
        }
    }

    for h in &mut highlights {
        let dur = h.end_time - h.start_time;

        if dur < min_d {
            // Force-extend symmetrically
            let deficit = min_d - dur;
            let extend_before = (deficit / 2.0).min(h.start_time); // don't go below 0
            let extend_after = deficit - extend_before;
            h.start_time -= extend_before;
            h.end_time = (h.end_time + extend_after).min(vd);

            // If still too short after extension (near video boundaries), extend the other side
            let new_dur = h.end_time - h.start_time;
            if new_dur < min_d {
                let still_need = min_d - new_dur;
                h.start_time = (h.start_time - still_need).max(0.0);
            }

            tracing::info!(
                "[{}] Extended '{}' from {:.1}s to {:.1}s (was below min {}s)",
                job_id, h.title, dur, h.end_time - h.start_time, min_d
            );
        }

        // Over the cap: trim from the START so the ending (the payoff/conclusion
        // that carries the meaning) is preserved rather than chopped off.
        if h.end_time - h.start_time > max_d {
            h.start_time = h.end_time - max_d;
        }
    }

    // Drop any that still can't meet min duration (video too short)
    highlights.retain(|h| {
        let dur = h.end_time - h.start_time;
        if dur < min_d {
            tracing::warn!(
                "[{}] Dropping '{}' ({:.1}s) — can't meet min {}s",
                job_id, h.title, dur, min_d
            );
            false
        } else {
            true
        }
    });

    if highlights.len() != before_count {
        tracing::info!(
            "[{}] Duration enforcement: {} → {} highlights",
            job_id, before_count, highlights.len()
        );
    }

    // The selector's overlap check runs before the reviewer's timestamp fixes,
    // padding and duration clamping — any of those can land two highlights on the
    // same span, yielding identical clips. Re-check here, after every mutation,
    // keeping the higher-scored clip of each clashing pair.
    let before_dedup = highlights.len();
    highlights.sort_by(|a, b| b.score.cmp(&a.score));
    let mut deduped: Vec<Highlight> = Vec::new();
    for h in highlights {
        let clashes = deduped.iter().any(|kept| {
            let overlap = (h.end_time.min(kept.end_time) - h.start_time.max(kept.start_time)).max(0.0);
            let shorter = (h.end_time - h.start_time).min(kept.end_time - kept.start_time);
            shorter > 0.0 && overlap / shorter > 0.5
        });
        if clashes {
            tracing::info!(
                "[{}] Dropping '{}' ({:.1}-{:.1}s) — duplicates a higher-scored clip",
                job_id, h.title, h.start_time, h.end_time
            );
        } else {
            deduped.push(h);
        }
    }
    highlights = deduped;
    highlights.sort_by(|a, b| a.start_time.partial_cmp(&b.start_time).unwrap_or(std::cmp::Ordering::Equal));
    if highlights.len() != before_dedup {
        tracing::info!(
            "[{}] Overlap dedup: {} → {} highlights",
            job_id, before_dedup, highlights.len()
        );
    }

    // Save enforced highlights to DB
    let _ = db::update_job_highlights(&state.db, job_id, &highlights);
    state.send_progress(job_id, "analyzing", 74,
        &format!("{} highlights ready (enforced {:.0}-{:.0}s)", highlights.len(), min_d, max_d));

    if highlights.is_empty() {
        let _ = db::update_job_status(&state.db, job_id, &JobStatus::Completed, 100);
        state.send_progress(job_id, "completed", 100, "No highlights after duration enforcement");
        return Ok(());
    }

    // 5. Cut clips
    state.send_progress(job_id, "cutting", 75, "Cutting clips");
    let total = highlights.len();
    let vertical = job.vertical_crop;
    let mut clips: Vec<ClipInfo> = Vec::new();

    for (i, h) in highlights.iter().enumerate() {
        let safe: String = h.title.chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .take(40).collect();
        let suffix = if vertical { "_9x16" } else { "" };
        let fname = format!("{:02}_{}{}{}.mp4", i + 1, safe, if safe.is_empty() { "" } else { "_" }, h.category);
        let fname = if vertical { fname.replace(".mp4", &format!("{}.mp4", suffix)) } else { fname };
        let clip_path = clips_dir.join(&fname);

        let vp = video_path.clone(); let cp = clip_path.clone();
        let (s, e) = (h.start_time, h.end_time);
        let expected_dur = e - s;

        match tokio::task::spawn_blocking(move || ffmpeg::cut_segment(&vp, &cp, s, e, vertical)).await? {
            Ok(actual_dur) => {
                // Verify the output file meets minimum duration
                if actual_dur < min_d - 1.0 {
                    tracing::warn!(
                        "[{}] clip {} '{}' actual duration {:.1}s < min {:.0}s (expected {:.1}s) — SKIPPED",
                        job_id, i + 1, h.title, actual_dur, min_d, expected_dur
                    );
                    let _ = std::fs::remove_file(&clip_path);
                    continue;
                }

                let sz = std::fs::metadata(&clip_path).map(|m| m.len()).unwrap_or(0);
                tracing::info!(
                    "[{}] clip {} OK: {:.1}s (expected {:.1}s), {:.1} MB",
                    job_id, i + 1, actual_dur, expected_dur, sz as f64 / 1e6
                );
                clips.push(ClipInfo { index: i + 1, filename: fname, highlight: h.clone(), file_size: sz });
            }
            Err(e) => tracing::warn!("[{}] clip {} failed: {}", job_id, i + 1, e),
        }
        let pct = 75 + ((i + 1) * 25 / total) as u8;
        state.send_progress(job_id, "cutting", pct, &format!("Clip {}/{}", i + 1, total));
    }

    std::fs::write(clips_dir.join("highlights.json"), serde_json::to_string_pretty(&highlights)?)?;
    let _ = db::update_job_completed(&state.db, job_id, &clips);
    state.send_progress(job_id, "completed", 100, &format!("{} clips ready", clips.len()));
    Ok(())
}
