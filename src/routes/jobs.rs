use axum::body::Body;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Multipart, Path, Query, State, WebSocketUpgrade};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use std::io::Write;
use tracing::info;
use uuid::Uuid;

use crate::db;
use crate::error::AppError;
use crate::models::*;
use crate::state::AppState;

/// POST /api/jobs — upload video, enqueue processing.
/// Streams file to disk chunk-by-chunk to avoid OOM on large files.
pub async fn create_job(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<JobResponse>), AppError> {
    let mut file_name: Option<String> = None;
    let mut language: Option<String> = None;
    let mut max_clips: Option<usize> = None;
    let mut min_clips: Option<usize> = None;
    let mut min_clip_duration: Option<u32> = None;
    let mut max_clip_duration: Option<u32> = None;
    let mut vertical_crop: Option<bool> = None;

    // Create a temp job dir to stream file into
    let tmp_id = Uuid::new_v4();
    let tmp_dir = state.job_dir(&tmp_id);
    std::fs::create_dir_all(&tmp_dir).map_err(|e| AppError::Internal(e.to_string()))?;

    let mut video_path: Option<std::path::PathBuf> = None;
    let mut total_written: u64 = 0;

    while let Some(field) = multipart.next_field().await
        .map_err(|e| AppError::BadRequest(format!("Multipart: {}", e)))? {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" | "video" => {
                file_name = field.file_name().map(|s| s.to_string());
                let fname = file_name.clone().unwrap_or_else(|| "video.mp4".into());
                let path = tmp_dir.join(&fname);

                // Stream to disk chunk by chunk
                let mut file = std::fs::File::create(&path)
                    .map_err(|e| AppError::Internal(format!("Create file: {}", e)))?;

                let mut stream = field;
                while let Some(chunk) = stream.chunk().await
                    .map_err(|e| AppError::BadRequest(format!("Read chunk: {}", e)))? {
                    total_written += chunk.len() as u64;

                    // Check size limit while streaming
                    let max_bytes = state.config.max_upload_mb * 1_000_000;
                    if total_written > max_bytes {
                        let _ = std::fs::remove_dir_all(&tmp_dir);
                        return Err(AppError::BadRequest(format!(
                            "File too large (>{} MB)", state.config.max_upload_mb
                        )));
                    }

                    file.write_all(&chunk)
                        .map_err(|e| AppError::Internal(format!("Write: {}", e)))?;
                }

                file.flush().map_err(|e| AppError::Internal(format!("Flush: {}", e)))?;
                video_path = Some(path);
            }
            "language" => { let v = field.text().await.unwrap_or_default(); if !v.is_empty() { language = Some(v); } }
            "max_clips" => { max_clips = field.text().await.ok().and_then(|v| v.parse().ok()); }
            "min_clips" => { min_clips = field.text().await.ok().and_then(|v| v.parse().ok()); }
            "min_clip_duration" => { min_clip_duration = field.text().await.ok().and_then(|v| v.parse().ok()); }
            "max_clip_duration" => { max_clip_duration = field.text().await.ok().and_then(|v| v.parse().ok()); }
            "vertical_crop" => { vertical_crop = field.text().await.ok().map(|v| v == "true" || v == "1"); }
            _ => {}
        }
    }

    let _video_path = video_path.ok_or_else(|| {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        AppError::BadRequest("Missing 'file' field".into())
    })?;
    let source_filename = file_name.unwrap_or_else(|| "video.mp4".into());

    let params = CreateJobParams {
        source_filename: source_filename.clone(),
        language, max_clips, min_clips, min_clip_duration, max_clip_duration, vertical_crop,
    };
    let defaults = JobDefaults {
        max_clips: state.config.max_clips,
        min_clips: state.config.min_clips,
        min_clip_duration: state.config.min_clip_duration,
        max_clip_duration: state.config.max_clip_duration,
    };
    let job = Job::new(params, &defaults);
    let job_id = job.id;
    let resp = JobResponse::from(&job);

    // Rename temp dir to actual job dir
    let real_dir = state.job_dir(&job_id);
    if tmp_dir != real_dir {
        std::fs::rename(&tmp_dir, &real_dir).map_err(|e| AppError::Internal(e.to_string()))?;
    }
    let real_video_path = real_dir.join(&source_filename);

    db::insert_job(&state.db, &job).map_err(|e| AppError::Internal(format!("DB: {}", e)))?;

    info!("Job {} queued: '{}' ({:.1} MB, streamed to disk)",
        job_id, source_filename, total_written as f64 / 1e6);

    state.get_or_create_progress_tx(&job_id);

    state.queue_tx.send(QueuedJob { job_id, video_path: real_video_path }).await
        .map_err(|_| AppError::Internal("Queue full".into()))?;

    Ok((StatusCode::ACCEPTED, Json(resp)))
}

/// GET /api/jobs
#[derive(Deserialize, Default)]
pub struct ListParams { pub status: Option<String> }

pub async fn list_jobs(
    State(state): State<AppState>,
    Query(p): Query<ListParams>,
) -> Result<Json<JobListResponse>, AppError> {
    let jobs = db::list_jobs(&state.db, p.status.as_deref())
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let responses: Vec<JobResponse> = jobs.iter().map(JobResponse::from).collect();
    let total = responses.len();
    Ok(Json(JobListResponse { jobs: responses, total }))
}

/// GET /api/jobs/:id
pub async fn get_job(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<JobDetailResponse>, AppError> {
    let job = db::get_job(&state.db, &id)
        .map_err(|e| AppError::Internal(e.to_string()))?
        .ok_or_else(|| AppError::NotFound(format!("Job {}", id)))?;
    Ok(Json(JobDetailResponse::from(&job)))
}

/// DELETE /api/jobs/:id
pub async fn delete_job(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let deleted = db::delete_job(&state.db, &id).map_err(|e| AppError::Internal(e.to_string()))?;
    if !deleted { return Err(AppError::NotFound(format!("Job {}", id))); }
    let dir = state.job_dir(&id);
    if dir.exists() { let _ = std::fs::remove_dir_all(&dir); }
    state.cleanup_progress(&id);
    info!("Deleted job {}", id);
    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/jobs/:id/clips/:filename
pub async fn download_clip(
    State(state): State<AppState>,
    Path((id, filename)): Path<(Uuid, String)>,
) -> Result<Response, AppError> {
    db::get_job(&state.db, &id).map_err(|e| AppError::Internal(e.to_string()))?
        .ok_or_else(|| AppError::NotFound(format!("Job {}", id)))?;

    // Reject path traversal — filename must be a bare name within the clips dir.
    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        return Err(AppError::BadRequest("Invalid filename".into()));
    }

    let path = state.clips_dir(&id).join(&filename);
    if !path.exists() { return Err(AppError::NotFound(format!("Clip '{}'", filename))); }
    stream_file(&path, &filename).await
}

/// Stream a file from disk as an attachment without loading it all into memory.
async fn stream_file(path: &std::path::Path, filename: &str) -> Result<Response, AppError> {
    let file = tokio::fs::File::open(path).await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let stream = tokio_util::io::ReaderStream::new(file);
    Response::builder()
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::CONTENT_LENGTH, len.to_string())
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}\"", filename))
        .body(Body::from_stream(stream))
        .map_err(|e| AppError::Internal(e.to_string()))
}

/// GET /api/jobs/:id/transcript
pub async fn get_transcript(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Response, AppError> {
    db::get_job(&state.db, &id).map_err(|e| AppError::Internal(e.to_string()))?
        .ok_or_else(|| AppError::NotFound(format!("Job {}", id)))?;
    let path = state.job_dir(&id).join("transcript.json");
    if !path.exists() { return Err(AppError::NotFound("Transcript not ready".into())); }
    let content = std::fs::read_to_string(&path).map_err(|e| AppError::Internal(e.to_string()))?;
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(content))
        .map_err(|e| AppError::Internal(e.to_string()))
}

/// GET /api/jobs/:id/ws — WebSocket live progress
pub async fn job_ws(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    ws: WebSocketUpgrade,
) -> Result<Response, AppError> {
    db::get_job(&state.db, &id).map_err(|e| AppError::Internal(e.to_string()))?
        .ok_or_else(|| AppError::NotFound(format!("Job {}", id)))?;

    let tx = state.get_or_create_progress_tx(&id);
    let mut rx = tx.subscribe();

    Ok(ws.on_upgrade(move |mut socket: WebSocket| async move {
        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(event) => {
                            let json = serde_json::to_string(&event).unwrap_or_default();
                            if socket.send(Message::Text(json)).await.is_err() { break; }
                            if event.status == "completed" || event.status == "failed" {
                                let _ = socket.send(Message::Close(None)).await;
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(_) => break,
                    }
                }
                msg = socket.recv() => {
                    match msg {
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Ok(Message::Ping(d))) => { let _ = socket.send(Message::Pong(d)).await; }
                        _ => {}
                    }
                }
            }
        }
    }))
}
