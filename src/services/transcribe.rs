use anyhow::{anyhow, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::{debug, info, warn};

use crate::config::AppConfig;
use crate::models::{Transcript, TranscriptSegment};
use crate::services::{ffmpeg, retry};

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    temperature: f64,
    max_tokens: u32,
}

#[derive(Serialize)]
struct Message {
    role: String,
    content: serde_json::Value,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMsg,
}

#[derive(Deserialize)]
struct ChoiceMsg {
    content: String,
}

/// Transcribe full audio via OpenRouter, chunking if needed
pub async fn transcribe_audio(
    audio_path: &Path,
    work_dir: &Path,
    config: &AppConfig,
    language: &Option<String>,
    client: &reqwest::Client,
) -> Result<Transcript> {
    let dur = ffmpeg::get_duration(audio_path)?;

    if dur <= config.chunk_duration as f64 + 30.0 {
        info!("Transcribing single file ({:.0}s)", dur);
        return transcribe_chunk(audio_path, 0.0, config, language, client).await;
    }

    info!("Audio {:.0}s → splitting into ~{}s chunks", dur, config.chunk_duration);
    let chunks = ffmpeg::split_audio_chunks(audio_path, config.chunk_duration, work_dir)?;
    info!("Processing {} chunks via OpenRouter STT", chunks.len());

    let mut all_segments = Vec::new();
    for (i, (chunk_path, offset)) in chunks.iter().enumerate() {
        info!("  Chunk {}/{} (offset {:.0}s)", i + 1, chunks.len(), offset);
        match transcribe_chunk(chunk_path, *offset, config, language, client).await {
            Ok(t) => all_segments.extend(t.segments),
            Err(e) => warn!("Chunk {} failed: {}", i + 1, e),
        }
        let _ = std::fs::remove_file(chunk_path);
    }

    all_segments.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
    let full_text = all_segments.iter().map(|s| s.text.trim().to_string()).collect::<Vec<_>>().join(" ");
    Ok(Transcript { segments: all_segments, full_text })
}

/// Transcribe one audio chunk via OpenRouter chat completions (multimodal)
async fn transcribe_chunk(
    audio_path: &Path,
    time_offset: f64,
    config: &AppConfig,
    language: &Option<String>,
    client: &reqwest::Client,
) -> Result<Transcript> {
    let audio_bytes = std::fs::read(audio_path).context("read audio")?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&audio_bytes);
    debug!("Audio chunk: {:.1}MB → base64", audio_bytes.len() as f64 / 1e6);

    let ext = audio_path.extension().and_then(|e| e.to_str()).unwrap_or("mp3");
    let mime = match ext { "wav" => "audio/wav", "ogg" => "audio/ogg", "m4a" => "audio/mp4", _ => "audio/mpeg" };

    let lang_hint = language.as_ref().map(|l| format!("The audio is in {l}. ")).unwrap_or_default();

    let system = format!(
r#"You are a precise speech-to-text transcription engine.
{lang_hint}Transcribe the provided audio with accurate timestamps.

Respond ONLY with a JSON object (no markdown fences, no commentary):
{{
  "segments": [
    {{"start": 0.0, "end": 3.5, "text": "First sentence"}},
    {{"start": 3.5, "end": 7.2, "text": "Second sentence"}}
  ]
}}

Rules:
- Timestamps in seconds relative to this clip's start
- Each segment = one sentence or phrase (3-30 seconds)
- Include ALL spoken content
- Preserve original language
- No speech → {{"segments": []}}"#);

    // Try input_audio format first (OpenAI-compatible)
    let user_content = serde_json::json!([
        {
            "type": "input_audio",
            "input_audio": { "data": b64, "format": ext }
        },
        { "type": "text", "text": "Transcribe this audio with timestamps. JSON only." }
    ]);

    let req = ChatRequest {
        model: config.stt_model.clone(),
        messages: vec![
            Message { role: "system".into(), content: serde_json::Value::String(system.clone()) },
            Message { role: "user".into(), content: user_content },
        ],
        temperature: 0.0,
        max_tokens: 8192,
    };

    let resp = retry::with_retry("OpenRouter STT", 4, || {
        client
            .post("https://openrouter.ai/api/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", config.openrouter_key))
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://github.com/video-clipper")
            .header("X-Title", "Video Clipper STT")
            .json(&req)
            .send()
    }).await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        // Fallback: try data-URL format (Gemini compatibility)
        if status.as_u16() == 400 || status.as_u16() == 422 {
            debug!("input_audio rejected ({}), trying data-URL fallback", status);
            let data_uri = format!("data:{};base64,{}", mime, b64);
            return transcribe_chunk_fallback(&data_uri, time_offset, &system, config, client).await;
        }
        return Err(anyhow!("OpenRouter STT {}: {}", status, safe_truncate(&body, 500)));
    }

    let chat: ChatResponse = resp.json().await.context("parse STT response")?;
    let content = chat.choices.first().map(|c| c.message.content.clone()).unwrap_or_default();
    parse_transcript(&content, time_offset)
}

/// Fallback: send audio as data-URL in image_url field (works with Gemini on OpenRouter)
async fn transcribe_chunk_fallback(
    data_uri: &str,
    time_offset: f64,
    system: &str,
    config: &AppConfig,
    client: &reqwest::Client,
) -> Result<Transcript> {
    let user_content = serde_json::json!([
        { "type": "image_url", "image_url": { "url": data_uri } },
        { "type": "text", "text": "Transcribe this audio with timestamps. JSON only." }
    ]);

    let req = ChatRequest {
        model: config.stt_model.clone(),
        messages: vec![
            Message { role: "system".into(), content: serde_json::Value::String(system.to_string()) },
            Message { role: "user".into(), content: user_content },
        ],
        temperature: 0.0,
        max_tokens: 8192,
    };

    let resp = retry::with_retry("OpenRouter STT fallback", 4, || {
        client
            .post("https://openrouter.ai/api/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", config.openrouter_key))
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://github.com/video-clipper")
            .json(&req)
            .send()
    }).await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("STT fallback {}: {}", status, safe_truncate(&body, 500)));
    }

    let chat: ChatResponse = resp.json().await?;
    let content = chat.choices.first().map(|c| c.message.content.clone()).unwrap_or_default();
    parse_transcript(&content, time_offset)
}

fn parse_transcript(content: &str, time_offset: f64) -> Result<Transcript> {
    let json_str = extract_json_object(content)?;

    #[derive(Deserialize)]
    struct R { segments: Vec<Seg> }
    #[derive(Deserialize)]
    struct Seg { start: f64, end: f64, text: String }

    let r: R = serde_json::from_str(&json_str).context("parse STT JSON")?;
    let segments: Vec<TranscriptSegment> = r.segments.into_iter()
        .filter(|s| !s.text.trim().is_empty())
        .map(|s| TranscriptSegment {
            start: s.start + time_offset,
            end: s.end + time_offset,
            text: s.text.trim().to_string(),
        })
        .collect();
    let full_text = segments.iter().map(|s| s.text.clone()).collect::<Vec<_>>().join(" ");
    Ok(Transcript { segments, full_text })
}

fn extract_json_object(text: &str) -> Result<String> {
    let t = text.trim();
    // Direct JSON
    if t.starts_with('{') {
        if let Some(end) = find_brace(t) { return Ok(t[..=end].to_string()); }
    }
    // Markdown fences
    for p in ["```json\n", "```JSON\n", "```\n"] {
        if let Some(s) = t.find(p) {
            let start = s + p.len();
            if let Some(end) = t[start..].find("```") {
                return Ok(t[start..start + end].trim().to_string());
            }
        }
    }
    // Brute force
    if let (Some(s), Some(e)) = (t.find('{'), t.rfind('}')) {
        if s < e { return Ok(t[s..=e].to_string()); }
    }
    Err(anyhow!("No JSON object in STT response"))
}

fn find_brace(s: &str) -> Option<usize> {
    let (mut d, mut q, mut esc) = (0i32, false, false);
    for (i, c) in s.char_indices() {
        if esc { esc = false; continue; }
        match c {
            '\\' if q => esc = true,
            '"' => q = !q,
            '{' if !q => d += 1,
            '}' if !q => { d -= 1; if d == 0 { return Some(i); } }
            _ => {}
        }
    }
    None
}

fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes { return s; }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) { end -= 1; }
    &s[..end]
}
