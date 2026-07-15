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

/// POST /api/style — upload video + style params
pub async fn create_style_job(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<StyleJobResponse>), AppError> {
    if state.config.fal_api_key.is_empty() {
        return Err(AppError::BadRequest("FAL_API_KEY not configured".into()));
    }

    let mut file_name: Option<String> = None;
    let mut prompt: Option<String> = None;
    let mut model: Option<String> = None;
    let mut strength: Option<f64> = None;

    let tmp_id = Uuid::new_v4();
    let tmp_dir = state.style_dir(&tmp_id);
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

                let mut file = std::fs::File::create(&path)
                    .map_err(|e| AppError::Internal(format!("Create: {}", e)))?;

                let mut stream = field;
                while let Some(chunk) = stream.chunk().await
                    .map_err(|e| AppError::BadRequest(format!("Read chunk: {}", e)))? {
                    total_written += chunk.len() as u64;
                    let max_bytes = state.config.max_upload_mb * 1_000_000;
                    if total_written > max_bytes {
                        let _ = std::fs::remove_dir_all(&tmp_dir);
                        return Err(AppError::BadRequest(format!("File too large (>{} MB)", state.config.max_upload_mb)));
                    }
                    file.write_all(&chunk).map_err(|e| AppError::Internal(format!("Write: {}", e)))?;
                }
                file.flush().map_err(|e| AppError::Internal(format!("Flush: {}", e)))?;
                video_path = Some(path);
            }
            "prompt" | "style_prompt" => {
                let v = field.text().await.unwrap_or_default();
                if !v.is_empty() { prompt = Some(v); }
            }
            "model" | "style_model" => {
                let v = field.text().await.unwrap_or_default();
                if !v.is_empty() { model = Some(v); }
            }
            "strength" | "style_strength" => {
                strength = field.text().await.ok().and_then(|v| v.parse().ok());
            }
            _ => {}
        }
    }

    let _video_path = video_path.ok_or_else(|| {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        AppError::BadRequest("Missing 'file'".into())
    })?;
    let prompt = prompt.ok_or_else(|| {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        AppError::BadRequest("Missing 'prompt'".into())
    })?;
    let source_filename = file_name.unwrap_or_else(|| "video.mp4".into());

    let now = chrono::Utc::now();
    let sj = StyleJob {
        id: Uuid::new_v4(),
        status: StyleStatus::Queued,
        progress: 0,
        source_filename: source_filename.clone(),
        prompt,
        model: model.unwrap_or_else(|| state.config.fal_style_model.clone()),
        strength: strength.unwrap_or(0.65),
        result_filename: None,
        result_size: None,
        error: None,
        created_at: now,
        updated_at: now,
    };

    let style_id = sj.id;
    let resp = StyleJobResponse::from(&sj);

    db::insert_style_job(&state.db, &sj).map_err(|e| AppError::Internal(format!("DB: {}", e)))?;

    // Rename temp dir to actual style dir
    let real_dir = state.style_dir(&style_id);
    if tmp_dir != real_dir {
        std::fs::rename(&tmp_dir, &real_dir).map_err(|e| AppError::Internal(e.to_string()))?;
    }
    let video_path = real_dir.join(&source_filename);

    info!("Style job {} queued: '{}' ({:.1} MB, streamed) → '{}'",
        style_id, source_filename, total_written as f64 / 1e6, sj.model);

    state.get_or_create_progress_tx(&style_id);

    state.style_queue_tx.send(QueuedStyleJob { style_id, video_path }).await
        .map_err(|_| AppError::Internal("Style queue full".into()))?;

    Ok((StatusCode::ACCEPTED, Json(resp)))
}

/// GET /api/style
#[derive(Deserialize, Default)]
pub struct ListParams { pub status: Option<String> }

pub async fn list_style_jobs(
    State(state): State<AppState>,
    Query(p): Query<ListParams>,
) -> Result<Json<StyleJobListResponse>, AppError> {
    let jobs = db::list_style_jobs(&state.db, p.status.as_deref())
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let responses: Vec<StyleJobResponse> = jobs.iter().map(StyleJobResponse::from).collect();
    let total = responses.len();
    Ok(Json(StyleJobListResponse { jobs: responses, total }))
}

/// GET /api/style/:id
pub async fn get_style_job(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<StyleJobResponse>, AppError> {
    let sj = db::get_style_job(&state.db, &id)
        .map_err(|e| AppError::Internal(e.to_string()))?
        .ok_or_else(|| AppError::NotFound(format!("Style job {}", id)))?;
    Ok(Json(StyleJobResponse::from(&sj)))
}

/// DELETE /api/style/:id
pub async fn delete_style_job(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let deleted = db::delete_style_job(&state.db, &id).map_err(|e| AppError::Internal(e.to_string()))?;
    if !deleted { return Err(AppError::NotFound(format!("Style job {}", id))); }
    let dir = state.style_dir(&id);
    if dir.exists() { let _ = std::fs::remove_dir_all(&dir); }
    state.cleanup_progress(&id);
    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/style/:id/result — download styled video
pub async fn download_result(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Response, AppError> {
    let sj = db::get_style_job(&state.db, &id)
        .map_err(|e| AppError::Internal(e.to_string()))?
        .ok_or_else(|| AppError::NotFound(format!("Style job {}", id)))?;

    let filename = sj.result_filename
        .ok_or_else(|| AppError::NotFound("Result not ready".into()))?;

    let path = state.style_dir(&id).join(&filename);
    if !path.exists() { return Err(AppError::NotFound("Result file not found".into())); }

    // Stream from disk instead of reading the whole video into memory.
    let file = tokio::fs::File::open(&path).await
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

/// GET /api/style/:id/ws — WebSocket live progress
pub async fn style_ws(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    ws: WebSocketUpgrade,
) -> Result<Response, AppError> {
    db::get_style_job(&state.db, &id)
        .map_err(|e| AppError::Internal(e.to_string()))?
        .ok_or_else(|| AppError::NotFound(format!("Style job {}", id)))?;

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
