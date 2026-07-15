use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Semaphore};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::db::DbPool;
use crate::models::{ProgressEvent, QueuedJob, QueuedStyleJob};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub db: DbPool,
    pub http_client: reqwest::Client,
    pub progress_tx: Arc<DashMap<Uuid, broadcast::Sender<ProgressEvent>>>,
    pub queue_tx: mpsc::Sender<QueuedJob>,
    pub style_queue_tx: mpsc::Sender<QueuedStyleJob>,
    pub semaphore: Arc<Semaphore>,
}

impl AppState {
    pub fn new(
        config: AppConfig,
        db: DbPool,
        queue_tx: mpsc::Sender<QueuedJob>,
        style_queue_tx: mpsc::Sender<QueuedStyleJob>,
    ) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent_jobs));
        Self {
            config: Arc::new(config), db, queue_tx, style_queue_tx, semaphore,
            http_client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build().expect("HTTP client"),
            progress_tx: Arc::new(DashMap::new()),
        }
    }

    pub fn job_dir(&self, id: &Uuid) -> std::path::PathBuf {
        std::path::PathBuf::from(&self.config.data_dir).join("jobs").join(id.to_string())
    }

    pub fn clips_dir(&self, id: &Uuid) -> std::path::PathBuf {
        self.job_dir(id).join("clips")
    }

    pub fn style_dir(&self, id: &Uuid) -> std::path::PathBuf {
        std::path::PathBuf::from(&self.config.data_dir).join("style").join(id.to_string())
    }

    pub fn get_or_create_progress_tx(&self, id: &Uuid) -> broadcast::Sender<ProgressEvent> {
        self.progress_tx.entry(*id).or_insert_with(|| broadcast::channel(64).0).clone()
    }

    pub fn send_progress(&self, id: &Uuid, status: &str, progress: u8, msg: &str) {
        if let Some(tx) = self.progress_tx.get(id) {
            let _ = tx.send(ProgressEvent {
                job_id: *id,
                status: status.to_string(),
                progress,
                message: msg.to_string(),
            });
        }
    }

    pub fn cleanup_progress(&self, id: &Uuid) {
        self.progress_tx.remove(id);
    }
}
