use anyhow::Result;
use std::path::PathBuf;
use tracing::{error, info};
use uuid::Uuid;

use crate::db;
use crate::models::StyleStatus;
use crate::services::styler;
use crate::state::AppState;

pub async fn run_style_pipeline(state: AppState, style_id: Uuid, video_path: PathBuf) {
    if let Err(e) = run_inner(&state, &style_id, &video_path).await {
        error!("[style:{}] Failed: {:?}", style_id, e);
        let _ = db::update_style_error(&state.db, &style_id, &e.to_string());
        state.send_progress(&style_id, "failed", 0, &e.to_string());
    }

    // Always delete source video after processing
    if video_path.exists() {
        let size = std::fs::metadata(&video_path).map(|m| m.len()).unwrap_or(0);
        match std::fs::remove_file(&video_path) {
            Ok(()) => info!("[style:{}] Deleted source video ({:.1} MB)", style_id, size as f64 / 1e6),
            Err(e) => tracing::warn!("[style:{}] Failed to delete source: {}", style_id, e),
        }
    }

    state.cleanup_progress(&style_id);
}

async fn run_inner(state: &AppState, id: &Uuid, video_path: &PathBuf) -> Result<()> {
    let sj = db::get_style_job(&state.db, id)?
        .ok_or_else(|| anyhow::anyhow!("Style job not found"))?;

    let out_dir = state.style_dir(id);
    std::fs::create_dir_all(&out_dir)?;

    let result_name = format!("styled_{}", sj.source_filename);
    let result_path = out_dir.join(&result_name);

    // 1. Upload
    let _ = db::update_style_status(&state.db, id, &StyleStatus::Uploading, 10);
    state.send_progress(id, "uploading", 10, "Uploading video to fal.ai");

    // 2-3. Process (upload + submit + poll happens inside styler::style_clip)
    let _ = db::update_style_status(&state.db, id, &StyleStatus::Processing, 30);
    state.send_progress(id, "processing", 30,
        &format!("Processing: {} (strength={:.2})", safe_truncate(&sj.prompt, 50), sj.strength));

    info!("[style:{}] model={} prompt='{}' strength={:.2}",
        id, sj.model, safe_truncate(&sj.prompt, 60), sj.strength);

    styler::style_clip(
        video_path, &result_path,
        &sj.prompt, sj.strength, &sj.model,
        &state.config, &state.http_client,
    ).await?;

    // 4. Done
    let _ = db::update_style_status(&state.db, id, &StyleStatus::Downloading, 90);
    state.send_progress(id, "downloading", 90, "Downloading result");

    let size = std::fs::metadata(&result_path).map(|m| m.len()).unwrap_or(0);
    let _ = db::update_style_completed(&state.db, id, &result_name, size);
    state.send_progress(id, "completed", 100,
        &format!("Done! {:.1} MB", size as f64 / 1e6));

    info!("[style:{}] Completed: {} ({:.1} MB)", id, result_name, size as f64 / 1e6);
    Ok(())
}

fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes { return s; }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) { end -= 1; }
    &s[..end]
}
