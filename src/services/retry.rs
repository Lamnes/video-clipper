use std::time::Duration;
use tracing::warn;

const MAX_BACKOFF_SECS: u64 = 30;

/// Retry a transient HTTP operation: network errors, HTTP 429, and 5xx responses.
/// `make_request` must build and send a *fresh* request on each call (the request
/// is re-sent on retry). Non-retryable responses (e.g. 400/422) are returned as-is
/// so the caller can handle them — only the transient classes are retried.
pub async fn with_retry<F, Fut>(
    label: &str,
    max_attempts: u32,
    mut make_request: F,
) -> anyhow::Result<reqwest::Response>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
{
    let mut attempt = 0;
    loop {
        attempt += 1;
        match make_request().await {
            Ok(resp) => {
                let status = resp.status();
                let retryable = status.as_u16() == 429 || status.is_server_error();
                if retryable && attempt < max_attempts {
                    let wait = retry_after(&resp).unwrap_or_else(|| backoff_secs(attempt));
                    warn!("{}: HTTP {} (attempt {}/{}), retrying in {}s", label, status, attempt, max_attempts, wait);
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    continue;
                }
                return Ok(resp);
            }
            Err(e) => {
                if attempt < max_attempts {
                    let wait = backoff_secs(attempt);
                    warn!("{}: network error '{}' (attempt {}/{}), retrying in {}s", label, e, attempt, max_attempts, wait);
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    continue;
                }
                return Err(anyhow::anyhow!("{} failed after {} attempts: {}", label, max_attempts, e));
            }
        }
    }
}

/// Exponential backoff: 2, 4, 8, ... seconds, capped.
fn backoff_secs(attempt: u32) -> u64 {
    2u64.saturating_pow(attempt).min(MAX_BACKOFF_SECS)
}

/// Honor a `Retry-After: <seconds>` header if the server sent one.
fn retry_after(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str().ok()?
        .trim()
        .parse::<u64>().ok()
        .map(|s| s.min(MAX_BACKOFF_SECS))
}
