use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "video-clipper", about = "AI-powered video highlight clipper API")]
pub struct AppConfig {
    #[arg(long, env = "HOST", default_value = "0.0.0.0")]
    pub host: String,

    #[arg(long, env = "PORT", default_value_t = 8080)]
    pub port: u16,

    #[arg(long, env = "OPENROUTER_API_KEY")]
    pub openrouter_key: String,

    #[arg(long, env = "STT_MODEL", default_value = "google/gemini-2.5-flash")]
    pub stt_model: String,

    #[arg(long, env = "ANALYSIS_MODEL", default_value = "anthropic/claude-sonnet-4")]
    pub analysis_model: String,

    /// LLM model for reviewing/validating highlights (second opinion)
    #[arg(long, env = "REVIEWER_MODEL", default_value = "google/gemini-2.5-flash")]
    pub reviewer_model: String,

    /// Max review rounds before accepting (0 = disable review)
    #[arg(long, env = "MAX_REVIEW_ROUNDS", default_value_t = 2)]
    pub max_review_rounds: u32,

    #[arg(long, env = "MAX_CLIP_DURATION", default_value_t = 90)]
    pub max_clip_duration: u32,

    /// Seconds of breathing room added before and after each clip so a thought
    /// doesn't start or end on an abrupt cut.
    #[arg(long, env = "CLIP_PADDING", default_value_t = 2.0)]
    pub clip_padding: f64,

    #[arg(long, env = "MIN_CLIP_DURATION", default_value_t = 10)]
    pub min_clip_duration: u32,

    #[arg(long, env = "MAX_CLIPS", default_value_t = 20)]
    pub max_clips: usize,

    /// Minimum number of clips to aim for (best-effort: capped by max_clips and by
    /// how much strong material the video actually has).
    #[arg(long, env = "MIN_CLIPS", default_value_t = 3)]
    pub min_clips: usize,

    #[arg(long, env = "CHUNK_DURATION", default_value_t = 300)]
    pub chunk_duration: u32,

    #[arg(long, env = "DATA_DIR", default_value = "./data")]
    pub data_dir: String,

    #[arg(long, env = "MAX_UPLOAD_MB", default_value_t = 2048)]
    pub max_upload_mb: u64,

    /// Max concurrent video processing jobs
    #[arg(long, env = "MAX_CONCURRENT_JOBS", default_value_t = 2)]
    pub max_concurrent_jobs: usize,

    /// Max concurrent style-transfer jobs. Separate from the clip limit: style jobs
    /// mostly idle waiting on fal.ai, so they must not hold clip-processing slots.
    #[arg(long, env = "MAX_CONCURRENT_STYLE_JOBS", default_value_t = 2)]
    pub max_concurrent_style_jobs: usize,

    /// SQLite database path (relative to DATA_DIR)
    #[arg(long, env = "DB_FILE", default_value = "clipper.db")]
    pub db_file: String,

    /// fal.ai API key (required for style transfer)
    #[arg(long, env = "FAL_API_KEY", default_value = "")]
    pub fal_api_key: String,

    /// Default fal.ai video-to-video model
    #[arg(long, env = "FAL_STYLE_MODEL", default_value = "fal-ai/wan/v2.1/video-to-video")]
    pub fal_style_model: String,

    /// Use OpenRouter Fusion (multi-model panel + judge) for highlight selection.
    /// More semantically robust moment picks, but costs several completions per call.
    #[arg(long, env = "ANALYSIS_FUSION", default_value_t = false)]
    pub analysis_fusion: bool,

    /// Fusion panel models, comma-separated (1-8). Empty = Fusion's default panel.
    #[arg(long, env = "FUSION_PANEL", default_value = "")]
    pub fusion_panel: String,

    /// Fusion judge model. Empty = Fusion's default judge.
    #[arg(long, env = "FUSION_JUDGE", default_value = "")]
    pub fusion_judge: String,

    /// Max tool-calling steps (web search/fetch) for the Fusion panel.
    #[arg(long, env = "FUSION_MAX_TOOL_CALLS", default_value_t = 2)]
    pub fusion_max_tool_calls: u32,
}
