use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::models::*;

pub type DbPool = Arc<Mutex<Connection>>;

pub fn init_db(path: &Path) -> Result<DbPool> {
    let conn = Connection::open(path).context("Failed to open SQLite")?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
    conn.execute_batch(SCHEMA)?;
    // Migration for DBs created before min_clips existed. Ignore the error if the
    // column is already present.
    let _ = conn.execute("ALTER TABLE jobs ADD COLUMN min_clips INTEGER NOT NULL DEFAULT 1", []);
    Ok(Arc::new(Mutex::new(conn)))
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS jobs (
    id                TEXT PRIMARY KEY,
    status            TEXT NOT NULL DEFAULT 'queued',
    progress          INTEGER NOT NULL DEFAULT 0,
    source_filename   TEXT NOT NULL,
    video_duration    REAL,
    language          TEXT,
    max_clips         INTEGER NOT NULL,
    min_clips         INTEGER NOT NULL DEFAULT 1,
    min_clip_duration INTEGER NOT NULL,
    max_clip_duration INTEGER NOT NULL,
    vertical_crop     INTEGER NOT NULL DEFAULT 0,
    highlights_json   TEXT NOT NULL DEFAULT '[]',
    clips_json        TEXT NOT NULL DEFAULT '[]',
    error             TEXT,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS style_jobs (
    id              TEXT PRIMARY KEY,
    status          TEXT NOT NULL DEFAULT 'queued',
    progress        INTEGER NOT NULL DEFAULT 0,
    source_filename TEXT NOT NULL,
    prompt          TEXT NOT NULL,
    model           TEXT NOT NULL,
    strength        REAL NOT NULL DEFAULT 0.65,
    result_filename TEXT,
    result_size     INTEGER,
    error           TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
"#;

/// On startup, fail any job left in a non-terminal state. The in-memory queue is
/// lost on restart and the source video is deleted after processing, so these
/// jobs can never resume — mark them failed instead of leaving them stuck.
/// Returns (clip_jobs_failed, style_jobs_failed).
pub fn fail_interrupted_jobs(db: &DbPool) -> Result<(usize, usize)> {
    let conn = db.lock().unwrap();
    let msg = "Interrupted by server restart";
    let jobs = conn.execute(
        "UPDATE jobs SET status='failed', error=?1, updated_at=?2
         WHERE status NOT IN ('completed','failed')",
        params![msg, now_str()],
    )?;
    let styles = conn.execute(
        "UPDATE style_jobs SET status='failed', error=?1, updated_at=?2
         WHERE status NOT IN ('completed','failed')",
        params![msg, now_str()],
    )?;
    Ok((jobs, styles))
}

// ════════════════════════════════════════════
//  Jobs CRUD
// ════════════════════════════════════════════

const JOB_COLS: &str =
    "id, status, progress, source_filename, video_duration, language,
     max_clips, min_clip_duration, max_clip_duration, vertical_crop,
     highlights_json, clips_json, error, created_at, updated_at, min_clips";

pub fn insert_job(db: &DbPool, job: &Job) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute(
        &format!("INSERT INTO jobs ({JOB_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)"),
        params![
            job.id.to_string(), job.status.as_str(), job.progress,
            job.source_filename, job.video_duration, job.language,
            job.max_clips as i64, job.min_clip_duration as i64, job.max_clip_duration as i64,
            job.vertical_crop as i64,
            serde_json::to_string(&job.highlights)?,
            serde_json::to_string(&job.clips)?,
            job.error, job.created_at.to_rfc3339(), job.updated_at.to_rfc3339(),
            job.min_clips as i64,
        ],
    )?;
    Ok(())
}

pub fn get_job(db: &DbPool, id: &Uuid) -> Result<Option<Job>> {
    let conn = db.lock().unwrap();
    let mut stmt = conn.prepare(&format!("SELECT {JOB_COLS} FROM jobs WHERE id=?1"))?;
    let mut rows = stmt.query(params![id.to_string()])?;
    match rows.next()? { Some(r) => Ok(Some(row_to_job(r)?)), None => Ok(None) }
}

pub fn list_jobs(db: &DbPool, status: Option<&str>) -> Result<Vec<Job>> {
    let conn = db.lock().unwrap();
    let mut jobs = Vec::new();
    if let Some(s) = status {
        let mut st = conn.prepare(&format!("SELECT {JOB_COLS} FROM jobs WHERE status=?1 ORDER BY created_at DESC"))?;
        let mut rows = st.query(params![s])?;
        while let Some(r) = rows.next()? { jobs.push(row_to_job(r)?); }
    } else {
        let mut st = conn.prepare(&format!("SELECT {JOB_COLS} FROM jobs ORDER BY created_at DESC"))?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? { jobs.push(row_to_job(r)?); }
    }
    Ok(jobs)
}

pub fn update_job_status(db: &DbPool, id: &Uuid, status: &JobStatus, progress: u8) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute("UPDATE jobs SET status=?1, progress=?2, updated_at=?3 WHERE id=?4",
        params![status.as_str(), progress, now_str(), id.to_string()])?;
    Ok(())
}

pub fn update_job_duration(db: &DbPool, id: &Uuid, d: f64) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute("UPDATE jobs SET video_duration=?1, updated_at=?2 WHERE id=?3",
        params![d, now_str(), id.to_string()])?;
    Ok(())
}

pub fn update_job_highlights(db: &DbPool, id: &Uuid, h: &[Highlight]) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute("UPDATE jobs SET highlights_json=?1, updated_at=?2 WHERE id=?3",
        params![serde_json::to_string(h)?, now_str(), id.to_string()])?;
    Ok(())
}

pub fn update_job_error(db: &DbPool, id: &Uuid, e: &str) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute("UPDATE jobs SET status='failed', error=?1, updated_at=?2 WHERE id=?3",
        params![e, now_str(), id.to_string()])?;
    Ok(())
}

pub fn update_job_completed(db: &DbPool, id: &Uuid, clips: &[ClipInfo]) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute("UPDATE jobs SET status='completed', progress=100, clips_json=?1, updated_at=?2 WHERE id=?3",
        params![serde_json::to_string(clips)?, now_str(), id.to_string()])?;
    Ok(())
}

pub fn delete_job(db: &DbPool, id: &Uuid) -> Result<bool> {
    let conn = db.lock().unwrap();
    Ok(conn.execute("DELETE FROM jobs WHERE id=?1", params![id.to_string()])? > 0)
}

fn row_to_job(r: &rusqlite::Row) -> Result<Job> {
    let id_s: String = r.get(0)?; let st_s: String = r.get(1)?;
    let hl_j: String = r.get(10)?; let cl_j: String = r.get(11)?;
    let cr_s: String = r.get(13)?; let up_s: String = r.get(14)?;
    Ok(Job {
        id: Uuid::parse_str(&id_s).unwrap_or_default(),
        status: JobStatus::from_str(&st_s),
        progress: r.get::<_, i32>(2)? as u8,
        source_filename: r.get(3)?, video_duration: r.get(4)?, language: r.get(5)?,
        max_clips: r.get::<_, i64>(6)? as usize,
        min_clips: r.get::<_, i64>(15)? as usize,
        min_clip_duration: r.get::<_, i64>(7)? as u32,
        max_clip_duration: r.get::<_, i64>(8)? as u32,
        vertical_crop: r.get::<_, i64>(9)? != 0,
        highlights: serde_json::from_str(&hl_j).unwrap_or_default(),
        clips: serde_json::from_str(&cl_j).unwrap_or_default(),
        error: r.get(12)?,
        created_at: parse_dt(&cr_s), updated_at: parse_dt(&up_s),
    })
}

// ════════════════════════════════════════════
//  Style Jobs CRUD
// ════════════════════════════════════════════

const STYLE_COLS: &str =
    "id, status, progress, source_filename, prompt, model, strength,
     result_filename, result_size, error, created_at, updated_at";

pub fn insert_style_job(db: &DbPool, sj: &StyleJob) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute(
        &format!("INSERT INTO style_jobs ({STYLE_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)"),
        params![
            sj.id.to_string(), sj.status.as_str(), sj.progress,
            sj.source_filename, sj.prompt, sj.model, sj.strength,
            sj.result_filename, sj.result_size.map(|s| s as i64),
            sj.error, sj.created_at.to_rfc3339(), sj.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_style_job(db: &DbPool, id: &Uuid) -> Result<Option<StyleJob>> {
    let conn = db.lock().unwrap();
    let mut stmt = conn.prepare(&format!("SELECT {STYLE_COLS} FROM style_jobs WHERE id=?1"))?;
    let mut rows = stmt.query(params![id.to_string()])?;
    match rows.next()? { Some(r) => Ok(Some(row_to_style(r)?)), None => Ok(None) }
}

pub fn list_style_jobs(db: &DbPool, status: Option<&str>) -> Result<Vec<StyleJob>> {
    let conn = db.lock().unwrap();
    let mut jobs = Vec::new();
    if let Some(s) = status {
        let mut st = conn.prepare(&format!("SELECT {STYLE_COLS} FROM style_jobs WHERE status=?1 ORDER BY created_at DESC"))?;
        let mut rows = st.query(params![s])?;
        while let Some(r) = rows.next()? { jobs.push(row_to_style(r)?); }
    } else {
        let mut st = conn.prepare(&format!("SELECT {STYLE_COLS} FROM style_jobs ORDER BY created_at DESC"))?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? { jobs.push(row_to_style(r)?); }
    }
    Ok(jobs)
}

pub fn update_style_status(db: &DbPool, id: &Uuid, status: &StyleStatus, progress: u8) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute("UPDATE style_jobs SET status=?1, progress=?2, updated_at=?3 WHERE id=?4",
        params![status.as_str(), progress, now_str(), id.to_string()])?;
    Ok(())
}

pub fn update_style_completed(db: &DbPool, id: &Uuid, filename: &str, size: u64) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute("UPDATE style_jobs SET status='completed', progress=100, result_filename=?1, result_size=?2, updated_at=?3 WHERE id=?4",
        params![filename, size as i64, now_str(), id.to_string()])?;
    Ok(())
}

pub fn update_style_error(db: &DbPool, id: &Uuid, e: &str) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute("UPDATE style_jobs SET status='failed', error=?1, updated_at=?2 WHERE id=?3",
        params![e, now_str(), id.to_string()])?;
    Ok(())
}

pub fn delete_style_job(db: &DbPool, id: &Uuid) -> Result<bool> {
    let conn = db.lock().unwrap();
    Ok(conn.execute("DELETE FROM style_jobs WHERE id=?1", params![id.to_string()])? > 0)
}

fn row_to_style(r: &rusqlite::Row) -> Result<StyleJob> {
    let id_s: String = r.get(0)?; let st_s: String = r.get(1)?;
    let cr_s: String = r.get(10)?; let up_s: String = r.get(11)?;
    Ok(StyleJob {
        id: Uuid::parse_str(&id_s).unwrap_or_default(),
        status: StyleStatus::from_str(&st_s),
        progress: r.get::<_, i32>(2)? as u8,
        source_filename: r.get(3)?, prompt: r.get(4)?, model: r.get(5)?,
        strength: r.get(6)?,
        result_filename: r.get(7)?,
        result_size: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
        error: r.get(9)?,
        created_at: parse_dt(&cr_s), updated_at: parse_dt(&up_s),
    })
}

// ── Helpers ──

fn now_str() -> String { chrono::Utc::now().to_rfc3339() }

fn parse_dt(s: &str) -> DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::Utc::now())
}

use chrono::DateTime;
