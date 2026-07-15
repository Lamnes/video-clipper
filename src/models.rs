use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ════════════════════════════════════════════
//  Clip Pipeline (jobs)
// ════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    ExtractingAudio,
    Transcribing,
    Analyzing,
    Cutting,
    Completed,
    Failed,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::ExtractingAudio => "extracting_audio",
            Self::Transcribing => "transcribing",
            Self::Analyzing => "analyzing",
            Self::Cutting => "cutting",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "queued" => Self::Queued,
            "extracting_audio" => Self::ExtractingAudio,
            "transcribing" => Self::Transcribing,
            "analyzing" => Self::Analyzing,
            "cutting" => Self::Cutting,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => Self::Queued,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: Uuid,
    pub status: JobStatus,
    pub progress: u8,
    pub source_filename: String,
    pub video_duration: Option<f64>,
    pub language: Option<String>,
    pub max_clips: usize,
    pub min_clips: usize,
    pub min_clip_duration: u32,
    pub max_clip_duration: u32,
    pub vertical_crop: bool,
    pub highlights: Vec<Highlight>,
    pub clips: Vec<ClipInfo>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Job {
    pub fn new(params: CreateJobParams, defaults: &JobDefaults) -> Self {
        let now = Utc::now();
        let max_clips = params.max_clips.unwrap_or(defaults.max_clips).max(1);
        // Minimum can't exceed the maximum, and is at least 1.
        let min_clips = params.min_clips.unwrap_or(defaults.min_clips).clamp(1, max_clips);
        Self {
            id: Uuid::new_v4(),
            status: JobStatus::Queued,
            progress: 0,
            source_filename: params.source_filename,
            video_duration: None,
            language: params.language,
            max_clips,
            min_clips,
            min_clip_duration: params.min_clip_duration.unwrap_or(defaults.min_clip_duration),
            max_clip_duration: params.max_clip_duration.unwrap_or(defaults.max_clip_duration),
            vertical_crop: params.vertical_crop.unwrap_or(false),
            highlights: Vec::new(),
            clips: Vec::new(),
            error: None,
            created_at: now,
            updated_at: now,
        }
    }
}

pub struct JobDefaults {
    pub max_clips: usize,
    pub min_clips: usize,
    pub min_clip_duration: u32,
    pub max_clip_duration: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    pub segments: Vec<TranscriptSegment>,
    pub full_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Highlight {
    pub title: String,
    pub category: String,
    pub start_time: f64,
    pub end_time: f64,
    pub description: String,
    pub score: u8,
    /// Reviewer verdict: "approved", "improved", "replaced"
    #[serde(default)]
    pub review_verdict: Option<String>,
    /// Reviewer's reasoning for the verdict
    #[serde(default)]
    pub review_reason: Option<String>,
    /// Reviewer's score (1-10)
    #[serde(default)]
    pub review_score: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipInfo {
    pub index: usize,
    pub filename: String,
    pub highlight: Highlight,
    pub file_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressEvent {
    pub job_id: Uuid,
    pub status: String,
    pub progress: u8,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateJobParams {
    #[serde(default)]
    pub source_filename: String,
    pub language: Option<String>,
    pub max_clips: Option<usize>,
    pub min_clips: Option<usize>,
    pub min_clip_duration: Option<u32>,
    pub max_clip_duration: Option<u32>,
    pub vertical_crop: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct JobResponse {
    pub id: Uuid,
    pub status: JobStatus,
    pub progress: u8,
    pub source_filename: String,
    pub video_duration: Option<f64>,
    pub vertical_crop: bool,
    pub highlights_count: usize,
    pub clips_count: usize,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<&Job> for JobResponse {
    fn from(j: &Job) -> Self {
        Self {
            id: j.id, status: j.status.clone(), progress: j.progress,
            source_filename: j.source_filename.clone(),
            video_duration: j.video_duration, vertical_crop: j.vertical_crop,
            highlights_count: j.highlights.len(), clips_count: j.clips.len(),
            error: j.error.clone(),
            created_at: j.created_at, updated_at: j.updated_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct JobDetailResponse {
    #[serde(flatten)]
    pub summary: JobResponse,
    pub highlights: Vec<Highlight>,
    pub clips: Vec<ClipInfo>,
}

impl From<&Job> for JobDetailResponse {
    fn from(j: &Job) -> Self {
        Self { summary: JobResponse::from(j), highlights: j.highlights.clone(), clips: j.clips.clone() }
    }
}

#[derive(Debug, Serialize)]
pub struct JobListResponse {
    pub jobs: Vec<JobResponse>,
    pub total: usize,
}

pub struct QueuedJob {
    pub job_id: Uuid,
    pub video_path: std::path::PathBuf,
}

// ════════════════════════════════════════════
//  Style Transfer (standalone feature)
// ════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StyleStatus {
    Queued,
    Uploading,
    Processing,
    Downloading,
    Completed,
    Failed,
}

impl StyleStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Uploading => "uploading",
            Self::Processing => "processing",
            Self::Downloading => "downloading",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "queued" => Self::Queued,
            "uploading" => Self::Uploading,
            "processing" => Self::Processing,
            "downloading" => Self::Downloading,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => Self::Queued,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StyleJob {
    pub id: Uuid,
    pub status: StyleStatus,
    pub progress: u8,
    pub source_filename: String,
    pub prompt: String,
    pub model: String,
    pub strength: f64,
    pub result_filename: Option<String>,
    pub result_size: Option<u64>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct StyleJobResponse {
    pub id: Uuid,
    pub status: StyleStatus,
    pub progress: u8,
    pub source_filename: String,
    pub prompt: String,
    pub model: String,
    pub strength: f64,
    pub result_filename: Option<String>,
    pub result_size: Option<u64>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<&StyleJob> for StyleJobResponse {
    fn from(s: &StyleJob) -> Self {
        Self {
            id: s.id, status: s.status.clone(), progress: s.progress,
            source_filename: s.source_filename.clone(),
            prompt: s.prompt.clone(), model: s.model.clone(), strength: s.strength,
            result_filename: s.result_filename.clone(), result_size: s.result_size,
            error: s.error.clone(),
            created_at: s.created_at, updated_at: s.updated_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct StyleJobListResponse {
    pub jobs: Vec<StyleJobResponse>,
    pub total: usize,
}

pub struct QueuedStyleJob {
    pub style_id: Uuid,
    pub video_path: std::path::PathBuf,
}
