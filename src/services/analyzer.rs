use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::AppConfig;
use crate::models::{Highlight, Transcript};
use crate::services::retry;
use crate::state::AppState;
use uuid::Uuid;

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Msg>,
    temperature: f64,
    max_tokens: u32,
    /// OpenRouter Fusion config (panel + judge). Omitted entirely when not used.
    #[serde(skip_serializing_if = "Option::is_none")]
    plugins: Option<serde_json::Value>,
}

/// Build the OpenRouter `plugins` block that configures Fusion. Panel/judge are
/// only included when explicitly set — otherwise Fusion falls back to its defaults.
fn fusion_plugins(config: &AppConfig) -> serde_json::Value {
    let mut plugin = serde_json::json!({
        "id": "fusion",
        "max_tool_calls": config.fusion_max_tool_calls,
    });
    let panel: Vec<&str> = config.fusion_panel
        .split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    if !panel.is_empty() {
        plugin["analysis_models"] = serde_json::json!(panel);
    }
    let judge = config.fusion_judge.trim();
    if !judge.is_empty() {
        plugin["model"] = serde_json::json!(judge);
    }
    serde_json::json!([plugin])
}

#[derive(Serialize)]
struct Msg { role: String, content: String }

#[derive(Deserialize)]
struct ChatResponse { choices: Vec<Choice> }
#[derive(Deserialize)]
struct Choice { message: ChoiceMsg }
#[derive(Deserialize)]
struct ChoiceMsg { content: String }

// ── Review result from the reviewer model ──

#[derive(Debug, Deserialize)]
struct ReviewResult {
    reviews: Vec<SingleReview>,
    #[serde(default)]
    overall_feedback: String,
}

#[derive(Debug, Deserialize)]
struct SingleReview {
    title: String,
    verdict: String, // "approved", "rejected", "needs_improvement"
    score: u8,
    reason: String,
    #[serde(default)]
    suggestion: String,
    /// Does the clip contain a complete thought/sentence?
    #[serde(default = "default_true")]
    thought_complete: bool,
    /// Recommended start time (reviewer can adjust)
    #[serde(default)]
    recommended_start: Option<f64>,
    /// Recommended end time (reviewer can adjust)
    #[serde(default)]
    recommended_end: Option<f64>,
}

fn default_true() -> bool { true }

/// Main entry point: analyze + review loop
pub async fn analyze_transcript(
    transcript: &Transcript,
    video_duration: f64,
    max_clips: usize,
    min_clips: usize,
    min_dur: u32,
    max_dur: u32,
    config: &AppConfig,
    client: &reqwest::Client,
    // callback to send progress updates
    state: &AppState,
    job_id: &Uuid,
) -> Result<Vec<Highlight>> {
    let ts_text = build_timestamped(transcript);
    let max_chars: usize = 48_000;
    let text = if ts_text.len() > max_chars {
        warn!("Transcript {} chars, truncating to {}", ts_text.len(), max_chars);
        safe_truncate(&ts_text, max_chars).to_string()
    } else { ts_text };

    // ── Round 1: Initial selection (retry once on a parse/transport failure) ──
    state.send_progress(job_id, "analyzing", 56, "Selecting highlights (round 1)");
    let mut highlights = match select_highlights(
        &text, video_duration, max_clips, min_clips, min_dur, max_dur,
        config, client, None,
    ).await {
        Ok(h) => h,
        Err(e) => {
            warn!("Initial selection failed ({e}) — retrying once");
            state.send_progress(job_id, "analyzing", 57, "Re-selecting highlights (retry)");
            select_highlights(
                &text, video_duration, max_clips, min_clips, min_dur, max_dur,
                config, client, None,
            ).await?
        }
    };

    info!("Initial selection: {} highlights", highlights.len());

    if highlights.is_empty() || config.max_review_rounds == 0 {
        return Ok(highlights);
    }

    // ── Review loop ──
    for round in 1..=config.max_review_rounds {
        state.send_progress(job_id, "analyzing", 60, &format!("Review round {}/{}", round, config.max_review_rounds));

        let review = review_highlights(
            &highlights, &text, video_duration, config, client,
        ).await;

        let review = match review {
            Ok(r) => r,
            Err(e) => {
                warn!("Review round {} failed: {} — accepting current highlights", round, e);
                break;
            }
        };

        // Count verdicts
        let approved = review.reviews.iter().filter(|r| r.verdict == "approved").count();
        let rejected = review.reviews.iter().filter(|r| r.verdict == "rejected").count();
        let needs_improve = review.reviews.iter().filter(|r| r.verdict == "needs_improvement").count();
        let incomplete_thoughts = review.reviews.iter().filter(|r| !r.thought_complete).count();

        info!(
            "Review round {}: {} approved, {} rejected, {} needs improvement, {} incomplete thoughts",
            round, approved, rejected, needs_improve, incomplete_thoughts
        );

        // Apply review results + timestamp corrections to highlights
        let mut any_timestamps_changed = false;
        for h in &mut highlights {
            if let Some(rev) = review.reviews.iter().find(|r| r.title == h.title) {
                h.review_verdict = Some(rev.verdict.clone());
                h.review_reason = Some(rev.reason.clone());
                h.review_score = Some(rev.score);

                // Apply reviewer's recommended timestamps if provided
                // (typically to extend clip so the thought/sentence is complete)
                if let (Some(new_start), Some(new_end)) = (rev.recommended_start, rev.recommended_end) {
                    let new_start = new_start.max(0.0);
                    let new_end = new_end.min(video_duration);
                    let new_dur = new_end - new_start;

                    // Sanity check: new timestamps should be reasonable
                    if new_start < new_end && new_dur >= min_dur as f64 && new_dur <= (max_dur as f64 * 1.5) {
                        info!(
                            "  '{}': reviewer adjusted {:.1}-{:.1}s → {:.1}-{:.1}s (thought_complete={})",
                            h.title, h.start_time, h.end_time, new_start, new_end, rev.thought_complete
                        );
                        h.start_time = new_start;
                        h.end_time = new_end;
                        any_timestamps_changed = true;

                        // If timestamps were fixed, upgrade verdict
                        if !rev.thought_complete {
                            h.review_verdict = Some("improved".into());
                            h.review_reason = Some(format!(
                                "Extended for thought completeness: {}",
                                rev.reason
                            ));
                        }
                    } else {
                        warn!(
                            "  '{}': reviewer suggested {:.1}-{:.1}s ({:.1}s) — rejected (out of bounds or too long)",
                            h.title, new_start, new_end, new_dur
                        );
                    }
                }
            }
        }

        // If all approved (possibly after timestamp fixes) — we're done
        if rejected == 0 && (needs_improve == 0 || any_timestamps_changed) {
            if any_timestamps_changed {
                info!("All issues resolved via timestamp adjustments — done");
            } else {
                info!("All highlights approved — done");
            }
            break;
        }

        // If this is the last round — accept what we have
        if round == config.max_review_rounds {
            // Drop rejected clips, but backfill from the best rejected ones if that
            // would leave us below the requested minimum (a weak clip beats a gap).
            let (mut keep, mut pool): (Vec<Highlight>, Vec<Highlight>) = highlights
                .drain(..)
                .partition(|h| h.review_verdict.as_deref() != Some("rejected"));
            if keep.len() < min_clips && !pool.is_empty() {
                pool.sort_by(|a, b| b.score.cmp(&a.score));
                let need = min_clips - keep.len();
                let taken: Vec<Highlight> = pool.into_iter().take(need).collect();
                info!("Backfilling {} rejected clip(s) to reach min_clips={}", taken.len(), min_clips);
                keep.extend(taken);
            }
            highlights = keep;
            info!("Final round — keeping {} highlights", highlights.len());
            break;
        }

        // ── Re-select with reviewer's feedback ──
        let feedback = build_feedback(&review, &highlights);
        state.send_progress(job_id, "analyzing", 63, &format!("Re-selecting with feedback (round {})", round + 1));

        let new_highlights = select_highlights(
            &text, video_duration, max_clips, min_clips, min_dur, max_dur,
            config, client, Some(&feedback),
        ).await;

        match new_highlights {
            Ok(h) if !h.is_empty() => {
                info!("Round {} re-selection: {} highlights", round + 1, h.len());
                highlights = h;
            }
            Ok(_) => {
                warn!("Re-selection returned empty — keeping previous");
                break;
            }
            Err(e) => {
                warn!("Re-selection failed: {} — keeping previous", e);
                break;
            }
        }
    }

    if highlights.len() < min_clips {
        warn!(
            "Only {} highlight(s) found — below requested minimum of {} (not enough strong material)",
            highlights.len(), min_clips
        );
    }

    Ok(highlights)
}

/// Call the analysis model to select highlights
async fn select_highlights(
    text: &str,
    video_duration: f64,
    max_clips: usize,
    min_clips: usize,
    min_dur: u32,
    max_dur: u32,
    config: &AppConfig,
    client: &reqwest::Client,
    reviewer_feedback: Option<&str>,
) -> Result<Vec<Highlight>> {
    let mut system = format!(
r#"You are a professional video editor AI. Analyze this timestamped transcript and find the most interesting, viral-worthy moments.

Categories (use exactly these labels):
- funny — humorous, comedic, laugh-out-loud
- dramatic — intense, high-tension, shocking
- emotional — touching, heartfelt, tear-jerking
- sad — melancholic, sorrowful
- inspirational — motivating, uplifting
- informative — key insights, "aha" moments
- controversial — provocative, debate-sparking
- action — exciting, fast-paced, high-energy
- awkward — cringe, uncomfortable but entertaining

Rules:
1. Clips MUST be {min_dur}-{max_dur} seconds
2. Select between {min_clips} and {max_clips} highlights — aim for AT LEAST {min_clips} strong moments, up to {max_clips}. If the material is thin, lower your bar to still reach {min_clips} rather than returning fewer.
3. Timestamps within 0-{video_duration:.1} seconds
4. Prefer standalone short-form clips (TikTok/Reels/Shorts worthy)
5. Add ~2s padding before/after key moment
6. Score 1-10 by viral potential
7. Sort by score descending
8. SKIP intros, outros, title cards, channel bumpers, sponsor/subscribe reads, and segments that are mostly silence, applause, or music without meaningful speech — these make weak standalone clips

**CRITICAL — THOUGHT COMPLETENESS:**
- Every clip MUST start at the BEGINNING of a sentence or thought
- Every clip MUST end AFTER the speaker FINISHES their point/sentence
- NEVER cut off mid-sentence, mid-word, or mid-idea
- Look at the transcript timestamps carefully: if a sentence continues past your end_time, EXTEND end_time to include the full sentence
- It's better to make a clip slightly longer than to cut off a thought

Respond ONLY with a JSON array (no markdown, no text):
[
  {{"title":"Short title","category":"funny","start_time":125.0,"end_time":155.0,"description":"What happens","score":9}}
]"#);

    if let Some(feedback) = reviewer_feedback {
        system.push_str(&format!(
            "\n\n## IMPORTANT: Reviewer feedback from previous round\nA second AI reviewer evaluated your previous selection and provided this feedback. Incorporate it:\n\n{}",
            feedback
        ));
    }

    // With Fusion on, a panel of models deliberates over the transcript and a judge
    // synthesizes — more semantically reliable moment picks than a single model.
    let (model, plugins) = if config.analysis_fusion {
        ("openrouter/fusion".to_string(), Some(fusion_plugins(config)))
    } else {
        (config.analysis_model.clone(), None)
    };

    let req = ChatRequest {
        model,
        messages: vec![
            Msg { role: "system".into(), content: system },
            Msg { role: "user".into(), content: format!("Timestamped transcript:\n\n{}", text) },
        ],
        temperature: 0.3,
        max_tokens: 8192,
        plugins,
    };

    let resp = retry::with_retry("OpenRouter analysis", 4, || {
        client
            .post("https://openrouter.ai/api/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", config.openrouter_key))
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://github.com/video-clipper")
            .header("X-Title", "Video Clipper Analyzer")
            .json(&req)
            .send()
    }).await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("OpenRouter {}: {}", status, safe_truncate(&body, 500)));
    }

    let chat: ChatResponse = resp.json().await.context("parse analysis")?;
    let content = chat.choices.first().map(|c| c.message.content.clone()).unwrap_or_default();
    debug!("Analysis: {} chars", content.len());

    let json_str = extract_json_array(&content)?;
    let mut highlights: Vec<Highlight> = parse_highlights(&json_str);
    if highlights.is_empty() {
        warn!("No parseable highlights. Raw response:\n{}", safe_truncate(&content, 1500));
        return Err(anyhow!("analysis returned no parseable highlights"));
    }

    // Post-process
    highlights.retain_mut(|h| {
        h.start_time = h.start_time.max(0.0);
        h.end_time = h.end_time.min(video_duration);
        let d = h.end_time - h.start_time;
        if d < min_dur as f64 {
            let need = min_dur as f64 - d;
            h.start_time = (h.start_time - need / 2.0).max(0.0);
            h.end_time = (h.end_time + need / 2.0).min(video_duration);
        }
        if d > max_dur as f64 { h.start_time = h.end_time - max_dur as f64; }
        h.end_time - h.start_time >= min_dur as f64 && h.start_time < h.end_time
    });

    highlights.sort_by(|a, b| b.score.cmp(&a.score));
    highlights.truncate(max_clips);

    // Remove overlaps
    let mut result: Vec<Highlight> = Vec::new();
    for h in highlights {
        if !result.iter().any(|ex| h.start_time < ex.end_time && h.end_time > ex.start_time) {
            result.push(h);
        }
    }
    result.sort_by(|a, b| a.start_time.partial_cmp(&b.start_time).unwrap());
    Ok(result)
}

/// Call the reviewer model to evaluate highlights
async fn review_highlights(
    highlights: &[Highlight],
    transcript_text: &str,
    video_duration: f64,
    config: &AppConfig,
    client: &reqwest::Client,
) -> Result<ReviewResult> {
    let highlights_json = serde_json::to_string_pretty(highlights)?;

    // Truncate transcript for reviewer (it needs context but less than selector)
    let transcript_preview = if transcript_text.len() > 20_000 {
        format!("{}...\n[truncated, {} chars total]", safe_truncate(transcript_text, 20_000), transcript_text.len())
    } else {
        transcript_text.to_string()
    };

    let system = format!(
r#"You are a senior content strategist reviewing video highlight selections. You have the full timestamped transcript to verify every clip.

## YOUR CRITICAL TASK: CHECK THOUGHT COMPLETENESS

For EACH highlight, read the transcript at those timestamps and check:

1. **THOUGHT COMPLETENESS** (most important):
   - Does the clip START at the beginning of a sentence/thought, not mid-sentence?
   - Does the clip END after the thought/sentence is FINISHED, not cut off mid-word or mid-idea?
   - If a person is making a point, does the clip include the FULL point including the conclusion?
   - Example of INCOMPLETE: clip ends at "and the reason this matters is because—" (cut off!)
   - Example of COMPLETE: clip ends at "...and that's why it changed everything." (natural ending)

2. **Standalone quality**: Would a viewer understand this clip without context?
3. **Category accuracy**: Is the label correct?
4. **Viral potential**: Score 1-10 for TikTok/Shorts

Video duration: {video_duration:.1} seconds.

## VERDICTS

- "approved" — thought is complete, timing is good, keep as-is
- "needs_improvement" — the moment is good BUT the thought is cut off or timing is wrong.
  YOU MUST provide `recommended_start` and `recommended_end` timestamps that fix the issue.
  Look at the transcript to find where the sentence/thought actually begins and ends.
- "rejected" — not interesting, OR the clip is an intro/outro/title card/sponsor read, or is mostly silence/applause/music with no meaningful speech. Suggest a better alternative with timestamps.

## RESPONSE FORMAT

Respond ONLY with a valid JSON object. No markdown, no thinking, no preamble. Start with {{:
{{
  "reviews": [
    {{
      "title": "Title of highlight",
      "verdict": "approved",
      "score": 8,
      "thought_complete": true,
      "reason": "The speaker finishes their point at 'and that changed everything'",
      "suggestion": "",
      "recommended_start": null,
      "recommended_end": null
    }},
    {{
      "title": "Another highlight",
      "verdict": "needs_improvement",
      "score": 7,
      "thought_complete": false,
      "reason": "Clip cuts off at 02:15 mid-sentence. Transcript shows the speaker finishes the thought at 02:28 with 'so that's the takeaway'",
      "suggestion": "Extend end to 02:30 to include the complete conclusion",
      "recommended_start": 120.0,
      "recommended_end": 150.0
    }}
  ],
  "overall_feedback": "General notes"
}}"#);

    let user_msg = format!(
        "## Highlights to review:\n{}\n\n## Transcript for context:\n{}",
        highlights_json, transcript_preview
    );

    let req = ChatRequest {
        model: config.reviewer_model.clone(),
        messages: vec![
            Msg { role: "system".into(), content: system },
            Msg { role: "user".into(), content: user_msg },
        ],
        temperature: 0.2,
        max_tokens: 4096,
        plugins: None,
    };

    info!("Sending {} highlights to reviewer ({})", highlights.len(), config.reviewer_model);

    let resp = retry::with_retry("OpenRouter reviewer", 4, || {
        client
            .post("https://openrouter.ai/api/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", config.openrouter_key))
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://github.com/video-clipper")
            .header("X-Title", "Video Clipper Reviewer")
            .json(&req)
            .send()
    }).await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Reviewer {}: {}", status, safe_truncate(&body, 500)));
    }

    let chat: ChatResponse = resp.json().await.context("parse reviewer response")?;
    let content = chat.choices.first().map(|c| c.message.content.clone()).unwrap_or_default();
    info!("Reviewer raw response: {} chars", content.len());

    // Try extract JSON, log raw content on failure for debugging
    let json_str = match extract_json_object(&content) {
        Ok(s) => s,
        Err(_) => {
            warn!("Reviewer JSON extraction failed. Raw response:\n{}", safe_truncate(&content, 1500));
            // Last resort: try to build a minimal valid response from whatever we got
            return build_fallback_review(highlights, &content);
        }
    };

    let result = match parse_reviews(&json_str) {
        Some(r) if !r.reviews.is_empty() => r,
        _ => {
            warn!("Reviewer JSON had no usable reviews. Extracted:\n{}", safe_truncate(&json_str, 1000));
            return build_fallback_review(highlights, &content);
        }
    };

    Ok(result)
}

/// Build human-readable feedback string from review results
fn build_feedback(review: &ReviewResult, highlights: &[Highlight]) -> String {
    let mut fb = String::new();

    for rev in &review.reviews {
        match rev.verdict.as_str() {
            "rejected" => {
                fb.push_str(&format!(
                    "REJECTED: \"{}\" (score {}/10) — {}\nSuggestion: {}\n\n",
                    rev.title, rev.score, rev.reason, rev.suggestion
                ));
            }
            "needs_improvement" => {
                if let Some(h) = highlights.iter().find(|h| h.title == rev.title) {
                    fb.push_str(&format!(
                        "IMPROVE: \"{}\" [{:.1}s-{:.1}s]",
                        rev.title, h.start_time, h.end_time,
                    ));
                    if !rev.thought_complete {
                        fb.push_str(" ⚠️ THOUGHT IS INCOMPLETE/CUT OFF!");
                    }
                    fb.push_str(&format!("\n  Reason: {}\n", rev.reason));
                    if rev.recommended_start.is_some() || rev.recommended_end.is_some() {
                        fb.push_str(&format!(
                            "  Recommended timestamps: {:.1}s - {:.1}s\n",
                            rev.recommended_start.unwrap_or(h.start_time),
                            rev.recommended_end.unwrap_or(h.end_time),
                        ));
                    }
                    fb.push_str(&format!("  Suggestion: {}\n\n", rev.suggestion));
                }
            }
            _ => {}
        }
    }

    // Summarize incomplete thoughts
    let incomplete: Vec<_> = review.reviews.iter().filter(|r| !r.thought_complete).collect();
    if !incomplete.is_empty() {
        fb.push_str(&format!(
            "⚠️ {} clips have INCOMPLETE THOUGHTS. You MUST extend these clips so the speaker finishes their sentence/point. Use the recommended timestamps above.\n\n",
            incomplete.len()
        ));
    }

    if !review.overall_feedback.is_empty() {
        fb.push_str(&format!("GENERAL FEEDBACK: {}\n", review.overall_feedback));
    }

    fb
}

/// Parse the reviewer response tolerantly. The reviewer model often echoes the
/// field names it was shown (`review_verdict`/`review_reason`/`review_score`)
/// instead of `verdict`/`reason`/`score`, and may add or omit keys. Pull values
/// by trying both naming conventions so a good review isn't discarded.
fn parse_reviews(json_obj: &str) -> Option<ReviewResult> {
    let v: serde_json::Value = serde_json::from_str(json_obj).ok()?;
    let arr = v.get("reviews")?.as_array()?;
    let mut reviews = Vec::new();
    for r in arr {
        let s = |keys: &[&str]| -> Option<String> {
            keys.iter().find_map(|k| r.get(*k).and_then(|x| x.as_str()).map(|t| t.to_string()))
        };
        let title = match s(&["title"]) { Some(t) if !t.is_empty() => t, _ => continue };
        let verdict = s(&["verdict", "review_verdict"]).unwrap_or_else(|| "approved".into());
        let reason = s(&["reason", "review_reason"]).unwrap_or_default();
        let suggestion = s(&["suggestion"]).unwrap_or_default();
        let score = ["score", "review_score"].iter()
            .find_map(|k| r.get(*k).and_then(|x| x.as_u64())).unwrap_or(0) as u8;
        let thought_complete = r.get("thought_complete").and_then(|x| x.as_bool()).unwrap_or(true);
        let recommended_start = r.get("recommended_start").and_then(|x| x.as_f64());
        let recommended_end = r.get("recommended_end").and_then(|x| x.as_f64());
        reviews.push(SingleReview {
            title, verdict, score, reason, suggestion,
            thought_complete, recommended_start, recommended_end,
        });
    }
    let overall_feedback = v.get("overall_feedback")
        .and_then(|x| x.as_str()).unwrap_or_default().to_string();
    Some(ReviewResult { reviews, overall_feedback })
}

/// If reviewer output can't be parsed, approve all highlights as fallback
fn build_fallback_review(highlights: &[Highlight], raw: &str) -> Result<ReviewResult> {
    warn!("Using fallback: approving all {} highlights", highlights.len());
    Ok(ReviewResult {
        reviews: highlights.iter().map(|h| SingleReview {
            title: h.title.clone(),
            verdict: "approved".into(),
            score: h.score,
            reason: "Auto-approved (reviewer response could not be parsed)".into(),
            suggestion: String::new(),
            thought_complete: true,
            recommended_start: None,
            recommended_end: None,
        }).collect(),
        overall_feedback: format!("Reviewer output was unparseable ({} chars). All highlights auto-approved.", raw.len()),
    })
}

/// Truncate string at a safe UTF-8 char boundary
fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ── Utility functions ──

fn build_timestamped(t: &Transcript) -> String {
    t.segments.iter()
        .map(|s| format!("[{} - {}] {}", fmt(s.start), fmt(s.end), s.text))
        .collect::<Vec<_>>().join("\n")
}

fn fmt(s: f64) -> String {
    let t = s as u64;
    let (h, m, sec) = (t / 3600, (t % 3600) / 60, t % 60);
    if h > 0 { format!("{:02}:{:02}:{:02}", h, m, sec) } else { format!("{:02}:{:02}", m, sec) }
}

/// Parse highlights tolerantly. A single malformed or truncated object (common
/// with LLM/Fusion output) must not throw away the whole batch: try a strict
/// parse first, then fall back to salvaging each complete `{...}` object in turn.
fn parse_highlights(json_array: &str) -> Vec<Highlight> {
    if let Ok(v) = serde_json::from_str::<Vec<Highlight>>(json_array) {
        return v;
    }
    warn!("Strict highlight parse failed — salvaging individual objects");
    let mut out = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = json_array[search_from..].find('{') {
        let start = search_from + rel;
        match find_bracket(&json_array[start..], '{', '}') {
            Some(rel_end) => {
                let end = start + rel_end;
                if let Ok(h) = serde_json::from_str::<Highlight>(&json_array[start..=end]) {
                    out.push(h);
                }
                search_from = end + 1;
            }
            // Unmatched brace = truncated final object; keep what we have.
            None => break,
        }
    }
    out
}

fn extract_json_array(text: &str) -> Result<String> {
    let stripped = strip_think_blocks(text);
    let t = stripped.trim();
    if t.starts_with('[') {
        if let Some(e) = find_bracket(t, '[', ']') { return Ok(t[..=e].to_string()); }
    }
    for p in ["```json\n", "```json\r\n", "```JSON\n", "```\n", "```\r\n"] {
        if let Some(s) = t.find(p) {
            let start = s + p.len();
            if let Some(end) = t[start..].find("```") {
                let inner = t[start..start + end].trim();
                if inner.starts_with('[') {
                    return Ok(inner.to_string());
                }
            }
        }
    }
    if let (Some(s), Some(e)) = (t.find('['), t.rfind(']')) {
        if s < e { return Ok(t[s..=e].to_string()); }
    }
    Err(anyhow!("No JSON array in response"))
}

fn extract_json_object(text: &str) -> Result<String> {
    // Strip <think>...</think> blocks (reasoning models like Qwen)
    let stripped = strip_think_blocks(text);
    let t = stripped.trim();

    // Direct JSON object
    if t.starts_with('{') {
        if let Some(e) = find_bracket(t, '{', '}') { return Ok(t[..=e].to_string()); }
    }

    // Markdown code fences: ```json ... ``` or ``` ... ```
    for p in ["```json\n", "```json\r\n", "```JSON\n", "```\n", "```\r\n"] {
        if let Some(s) = t.find(p) {
            let start = s + p.len();
            if let Some(end) = t[start..].find("```") {
                let inner = t[start..start + end].trim();
                if inner.starts_with('{') {
                    return Ok(inner.to_string());
                }
            }
        }
    }

    // Find first { and last } anywhere in the text
    if let (Some(s), Some(e)) = (t.find('{'), t.rfind('}')) {
        if s < e {
            let candidate = &t[s..=e];
            // Validate it's parseable
            if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
                return Ok(candidate.to_string());
            }
        }
    }

    Err(anyhow!("No JSON object in response"))
}

fn strip_think_blocks(text: &str) -> String {
    let mut result = text.to_string();
    // Remove <think>...</think> blocks (Qwen, DeepSeek thinking models)
    while let Some(start) = result.find("<think>") {
        if let Some(end) = result.find("</think>") {
            let block_end = end + "</think>".len();
            result = format!("{}{}", &result[..start], &result[block_end..]);
        } else {
            // Unclosed <think> — remove everything from <think> to end
            result = result[..start].to_string();
            break;
        }
    }
    result
}

fn find_bracket(s: &str, open: char, close: char) -> Option<usize> {
    let (mut d, mut q, mut esc) = (0i32, false, false);
    for (i, c) in s.char_indices() {
        if esc { esc = false; continue; }
        match c {
            '\\' if q => esc = true,
            '"' => q = !q,
            c2 if c2 == open && !q => d += 1,
            c2 if c2 == close && !q => { d -= 1; if d == 0 { return Some(i); } }
            _ => {}
        }
    }
    None
}
