mod config;
mod db;
mod error;
mod models;
mod routes;
mod services;
mod state;

use axum::extract::DefaultBodyLimit;
use axum::response::{Html, IntoResponse};
use axum::routing::{delete, get, post};
use axum::Router;
use clap::Parser;
use std::sync::Arc;
use tokio::sync::mpsc;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;

use config::AppConfig;
use models::{QueuedJob, QueuedStyleJob};
use services::{pipeline, style_pipeline};
use state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = AppConfig::parse();
    services::ffmpeg::check_ffmpeg()?;
    info!("FFmpeg: OK");

    std::fs::create_dir_all(&config.data_dir)?;
    std::fs::create_dir_all(format!("{}/jobs", config.data_dir))?;
    std::fs::create_dir_all(format!("{}/style", config.data_dir))?;

    let db_path = std::path::PathBuf::from(&config.data_dir).join(&config.db_file);
    let db = db::init_db(&db_path)?;
    info!("SQLite: {}", db_path.display());

    // Fail any jobs left running from a previous run — their queue entries and
    // source videos are gone, so they can't resume.
    match db::fail_interrupted_jobs(&db) {
        Ok((j, s)) if j + s > 0 => info!("Marked {} clip + {} style interrupted job(s) as failed", j, s),
        Ok(_) => {}
        Err(e) => tracing::warn!("Could not clean up interrupted jobs: {}", e),
    }

    let (queue_tx, queue_rx) = mpsc::channel::<QueuedJob>(100);
    let (style_tx, style_rx) = mpsc::channel::<QueuedStyleJob>(100);

    let bind = format!("{}:{}", config.host, config.port);
    let max_upload = config.max_upload_mb * 1_000_000;

    info!("STT model:       {}", config.stt_model);
    if config.analysis_fusion {
        let panel = if config.fusion_panel.trim().is_empty() { "default panel".to_string() } else { config.fusion_panel.clone() };
        info!("Analysis model:  openrouter/fusion ({})", panel);
    } else {
        info!("Analysis model:  {}", config.analysis_model);
    }
    info!("Reviewer model:  {} ({} rounds)", config.reviewer_model, config.max_review_rounds);
    info!("Style model:     {}", config.fal_style_model);
    info!("Concurrency:     {} clip / {} style", config.max_concurrent_jobs, config.max_concurrent_style_jobs);
    info!("fal.ai:          {}", if config.fal_api_key.is_empty() { "NOT configured" } else { "OK" });

    let state = AppState::new(config, db, queue_tx, style_tx);

    // Spawn queue workers
    let ws = state.clone();
    tokio::spawn(queue_worker(ws, queue_rx));
    let ws2 = state.clone();
    tokio::spawn(style_queue_worker(ws2, style_rx));

    let app = Router::new()
        .route("/health", get(routes::health::health))
        // Frontend
        .route("/", get(serve_frontend))
        // Clip pipeline
        .route("/api/jobs", post(routes::jobs::create_job))
        .route("/api/jobs", get(routes::jobs::list_jobs))
        .route("/api/jobs/:id", get(routes::jobs::get_job))
        .route("/api/jobs/:id", delete(routes::jobs::delete_job))
        .route("/api/jobs/:id/clips/:filename", get(routes::jobs::download_clip))
        .route("/api/jobs/:id/transcript", get(routes::jobs::get_transcript))
        .route("/api/jobs/:id/ws", get(routes::jobs::job_ws))
        // Style transfer (separate feature)
        .route("/api/style", post(routes::style::create_style_job))
        .route("/api/style", get(routes::style::list_style_jobs))
        .route("/api/style/:id", get(routes::style::get_style_job))
        .route("/api/style/:id", delete(routes::style::delete_style_job))
        .route("/api/style/:id/result", get(routes::style::download_result))
        .route("/api/style/:id/ws", get(routes::style::style_ws))
        .layer(DefaultBodyLimit::disable())
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    info!("Video Clipper API on {}", bind);
    info!("Frontend: http://{}", bind);
    info!("Max upload: {} MB (streamed to disk)", max_upload / 1_000_000);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn queue_worker(state: AppState, mut rx: mpsc::Receiver<QueuedJob>) {
    info!("Clip queue worker started (max concurrent: {})", state.semaphore.available_permits());
    while let Some(job) = rx.recv().await {
        let state = state.clone();
        let sem = Arc::clone(&state.semaphore);
        tokio::spawn(async move {
            let _permit = match sem.acquire().await { Ok(p) => p, Err(_) => return };
            pipeline::run_pipeline(state, job.job_id, job.video_path).await;
        });
    }
}

async fn style_queue_worker(state: AppState, mut rx: mpsc::Receiver<QueuedStyleJob>) {
    info!("Style queue worker started (max concurrent: {})", state.style_semaphore.available_permits());
    while let Some(job) = rx.recv().await {
        let state = state.clone();
        // Own semaphore — a style job waiting on fal.ai must not block clip cutting.
        let sem = Arc::clone(&state.style_semaphore);
        tokio::spawn(async move {
            let _permit = match sem.acquire().await { Ok(p) => p, Err(_) => return };
            style_pipeline::run_style_pipeline(state, job.style_id, job.video_path).await;
        });
    }
}

/// Serve embedded frontend
async fn serve_frontend() -> impl IntoResponse {
    Html(include_str!("../static/index.html"))
}
