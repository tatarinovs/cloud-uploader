use anyhow::{bail, Context, Result};
use reqwest::{header, Client, Response, StatusCode};
use std::path::Path;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

pub const DEFAULT_USER_AGENT: &str = "cloud-uploader/1.0 (+https://github.com/cloud-uploader)";

pub fn build_http_client(extra_timeout: Option<Duration>) -> Result<Client> {
    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::USER_AGENT,
        header::HeaderValue::from_static(DEFAULT_USER_AGENT),
    );

    Client::builder()
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(15))
        .timeout(extra_timeout.unwrap_or(Duration::from_secs(600)))
        .build()
        .context("Failed to build reqwest HTTP client")
}

pub async fn retry_request<F, Fut>(op_name: &str, mut f: F) -> Result<Response>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<Response, reqwest::Error>>,
{
    let max_retries = 3;
    let mut backoff = Duration::from_secs(1);

    for attempt in 1..=max_retries {
        match f().await {
            Ok(resp) => {
                let status = resp.status();
                if (status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS)
                    && attempt < max_retries
                {
                    warn!(
                        "[{}] Server returned status {} on attempt {}/{}. Retrying in {:?}...",
                        op_name, status, attempt, max_retries, backoff
                    );
                    tokio::time::sleep(backoff).await;
                    backoff *= 2;
                    continue;
                }
                return Ok(resp);
            }
            Err(err) => {
                if attempt < max_retries
                    && (err.is_timeout() || err.is_connect() || err.is_request())
                {
                    warn!(
                        "[{}] Network error on attempt {}/{}: {}. Retrying in {:?}...",
                        op_name, attempt, max_retries, err, backoff
                    );
                    tokio::time::sleep(backoff).await;
                    backoff *= 2;
                } else {
                    return Err(anyhow::Error::new(err).context(format!(
                        "[{}] Request failed permanently on attempt {}",
                        op_name, attempt
                    )));
                }
            }
        }
    }

    bail!("[{}] Failed after {} retries", op_name, max_retries);
}

pub async fn stream_download_to_file(mut response: Response, destination: &Path) -> Result<u64> {
    if !response.status().is_success() {
        bail!("Download failed with status: {}", response.status());
    }

    let mut file = File::create(destination).await.with_context(|| {
        format!(
            "Failed to create destination file: {}",
            destination.display()
        )
    })?;

    let mut total_bytes = 0u64;
    while let Some(chunk_result) = response
        .chunk()
        .await
        .context("Error reading response stream")?
    {
        file.write_all(&chunk_result)
            .await
            .context("Error writing chunk to file")?;
        total_bytes += chunk_result.len() as u64;
    }

    file.flush()
        .await
        .context("Error flushing destination file")?;
    debug!(
        "Successfully downloaded {} bytes to {}",
        total_bytes,
        destination.display()
    );
    Ok(total_bytes)
}
