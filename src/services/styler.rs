use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::{debug, info, warn};

use crate::config::AppConfig;
use crate::services::retry;

// ── fal.ai API types ──

#[derive(Serialize)]
struct FalUploadInitiate {
    file_name: String,
    content_type: String,
}

#[derive(Deserialize)]
struct FalUploadResponse {
    upload_url: String,
    file_url: String,
}

#[derive(Serialize)]
struct FalVideoRequest {
    video_url: String,
    prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    strength: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    negative_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_inference_steps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u64>,
}

#[derive(Deserialize)]
struct FalQueueResponse {
    request_id: String,
}

#[derive(Deserialize)]
struct FalStatusResponse {
    status: String,
    #[serde(default)]
    #[allow(dead_code)]
    response_url: Option<String>,
}

#[derive(Deserialize)]
struct FalResultResponse {
    #[serde(default)]
    video: Option<FalVideo>,
    // Some models nest output differently
    #[serde(default)]
    output: Option<FalOutput>,
}

#[derive(Deserialize)]
struct FalVideo {
    url: String,
}

#[derive(Deserialize)]
struct FalOutput {
    #[serde(default)]
    video: Option<FalVideo>,
    #[serde(default)]
    url: Option<String>,
}

/// Style a single video clip via fal.ai video-to-video model.
/// Returns the path to the styled output file.
pub async fn style_clip(
    clip_path: &Path,
    output_path: &Path,
    prompt: &str,
    strength: f64,
    model: &str,
    config: &AppConfig,
    client: &reqwest::Client,
) -> Result<()> {
    if config.fal_api_key.is_empty() {
        return Err(anyhow!("FAL_API_KEY not set"));
    }

    // Step 1: Upload clip to fal.ai storage
    info!("  Uploading clip to fal.ai storage");
    let video_url = upload_to_fal(clip_path, config, client).await?;
    debug!("  fal.ai URL: {}", video_url);

    // Step 2: Submit video-to-video job
    info!("  Submitting to {} with prompt: '{}'", model, safe_truncate(prompt, 60));
    let request_id = submit_job(model, &video_url, prompt, strength, config, client).await?;
    debug!("  Request ID: {}", request_id);

    // Step 3: Poll until complete
    info!("  Waiting for fal.ai processing...");
    let result_url = poll_until_done(model, &request_id, config, client).await?;

    // Step 4: Download result
    info!("  Downloading styled video");
    download_result(&result_url, output_path, client).await?;

    let size = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    info!("  Styled clip saved: {:.1} MB", size as f64 / 1e6);

    Ok(())
}

/// Upload a file to fal.ai storage, returns the accessible URL
async fn upload_to_fal(
    file_path: &Path,
    config: &AppConfig,
    client: &reqwest::Client,
) -> Result<String> {
    let filename = file_path.file_name().unwrap_or_default().to_string_lossy().to_string();
    let bytes = std::fs::read(file_path).context("read clip for upload")?;

    // Initiate upload
    let init_resp = retry::with_retry("fal.ai upload initiate", 4, || {
        client
            .post("https://rest.alpha.fal.ai/storage/upload/initiate")
            .header("Authorization", format!("Key {}", config.fal_api_key))
            .json(&FalUploadInitiate {
                file_name: filename.clone(),
                content_type: "video/mp4".into(),
            })
            .send()
    }).await?;

    if !init_resp.status().is_success() {
        let body = init_resp.text().await.unwrap_or_default();
        return Err(anyhow!("fal.ai upload initiate {}", safe_truncate(&body, 300)));
    }

    let upload_info: FalUploadResponse = init_resp.json().await.context("parse upload response")?;

    // Upload the actual file
    let put_resp = client
        .put(&upload_info.upload_url)
        .header("Content-Type", "video/mp4")
        .body(bytes)
        .send()
        .await
        .context("fal.ai file upload failed")?;

    if !put_resp.status().is_success() {
        return Err(anyhow!("fal.ai PUT upload failed: {}", put_resp.status()));
    }

    Ok(upload_info.file_url)
}

/// Submit a video-to-video job to fal.ai queue
async fn submit_job(
    model: &str,
    video_url: &str,
    prompt: &str,
    strength: f64,
    config: &AppConfig,
    client: &reqwest::Client,
) -> Result<String> {
    let url = format!("https://queue.fal.run/{}", model);

    let req = FalVideoRequest {
        video_url: video_url.into(),
        prompt: prompt.into(),
        strength: Some(strength),
        negative_prompt: Some("blurry, low quality, distorted, watermark".into()),
        num_inference_steps: Some(30),
        seed: None,
    };

    let resp = retry::with_retry("fal.ai queue submit", 4, || {
        client
            .post(&url)
            .header("Authorization", format!("Key {}", config.fal_api_key))
            .header("Content-Type", "application/json")
            .json(&req)
            .send()
    }).await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("fal.ai submit {}: {}", status, safe_truncate(&body, 300)));
    }

    let queue_resp: FalQueueResponse = resp.json().await.context("parse queue response")?;
    Ok(queue_resp.request_id)
}

/// Poll fal.ai queue until the job is done. Returns the result video URL.
async fn poll_until_done(
    model: &str,
    request_id: &str,
    config: &AppConfig,
    client: &reqwest::Client,
) -> Result<String> {
    let status_url = format!(
        "https://queue.fal.run/{}/requests/{}/status",
        model, request_id
    );
    let result_url_base = format!(
        "https://queue.fal.run/{}/requests/{}",
        model, request_id
    );

    let max_polls = 180; // 15 minutes at 5s intervals
    for i in 0..max_polls {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let resp = client
            .get(&status_url)
            .header("Authorization", format!("Key {}", config.fal_api_key))
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                warn!("  Poll {} failed: {}", i, e);
                continue;
            }
        };

        if !resp.status().is_success() {
            debug!("  Poll {} status: {}", i, resp.status());
            continue;
        }

        let status: FalStatusResponse = match resp.json().await {
            Ok(s) => s,
            Err(e) => {
                debug!("  Poll {} parse error: {}", i, e);
                continue;
            }
        };

        debug!("  Poll {}: status={}", i, status.status);

        match status.status.as_str() {
            "COMPLETED" => {
                // Fetch the full result
                let result_resp = client
                    .get(&result_url_base)
                    .header("Authorization", format!("Key {}", config.fal_api_key))
                    .send()
                    .await
                    .context("fetch fal.ai result")?;

                if !result_resp.status().is_success() {
                    let body = result_resp.text().await.unwrap_or_default();
                    return Err(anyhow!("fal.ai result fetch failed: {}", safe_truncate(&body, 300)));
                }

                let result: FalResultResponse = result_resp.json().await
                    .context("parse fal.ai result")?;

                // Try to find the video URL in various response formats
                if let Some(v) = result.video {
                    return Ok(v.url);
                }
                if let Some(o) = result.output {
                    if let Some(v) = o.video {
                        return Ok(v.url);
                    }
                    if let Some(u) = o.url {
                        return Ok(u);
                    }
                }

                return Err(anyhow!("fal.ai completed but no video URL in response"));
            }
            "FAILED" => {
                return Err(anyhow!("fal.ai job failed"));
            }
            _ => {} // IN_QUEUE, IN_PROGRESS — keep polling
        }
    }

    Err(anyhow!("fal.ai job timed out after {} polls", max_polls))
}

/// Download the result video from fal.ai CDN
async fn download_result(
    url: &str,
    output_path: &Path,
    client: &reqwest::Client,
) -> Result<()> {
    let resp = retry::with_retry("download styled video", 4, || client.get(url).send()).await?;

    if !resp.status().is_success() {
        return Err(anyhow!("Download failed: {}", resp.status()));
    }

    let bytes = resp.bytes().await.context("read response body")?;
    std::fs::write(output_path, &bytes).context("write styled video")?;
    Ok(())
}

fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes { return s; }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) { end -= 1; }
    &s[..end]
}
