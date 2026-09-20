use crate::client::{build_http_client, retry_request};
use crate::storage::{RemoteFileInfo, StorageProvider};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;
use tokio::fs::File;
use url::form_urlencoded;

const YA_UPLOAD_API: &str = "https://cloud-api.yandex.net/v1/disk/resources/upload";
const YA_DOWNLOAD_API: &str = "https://cloud-api.yandex.net/v1/disk/resources/download";
const YA_RESOURCE_API: &str = "https://cloud-api.yandex.net/v1/disk/resources";

pub struct YandexDiskProvider {
    token: String,
    client: Client,
}

impl YandexDiskProvider {
    pub fn new(token: String) -> Result<Self> {
        Ok(Self {
            token,
            client: build_http_client(Some(Duration::from_secs(600)))?,
        })
    }

    fn auth_header(&self) -> String {
        format!("OAuth {}", self.token)
    }
}

#[derive(Deserialize)]
struct YaApiResponse {
    href: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    method: String,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct YaResourceResponse {
    name: Option<String>,
    path: Option<String>,
    #[serde(rename = "type")]
    item_type: Option<String>,
    size: Option<i64>,
    md5: Option<String>,
    modified: Option<String>,
    _embedded: Option<YaEmbedded>,
}

#[derive(Deserialize)]
struct YaEmbedded {
    items: Vec<YaResourceItem>,
}

#[derive(Deserialize)]
struct YaResourceItem {
    name: String,
    path: String,
    #[serde(rename = "type")]
    item_type: String,
    #[serde(default)]
    size: i64,
    md5: Option<String>,
    modified: Option<String>,
}

#[async_trait]
impl StorageProvider for YandexDiskProvider {
    fn name(&self) -> &'static str {
        "Yandex Disk"
    }

    async fn ensure_dir(&self, remote_dir: &str) -> Result<()> {
        let clean = remote_dir.trim();
        if clean.is_empty() || clean == "/" {
            return Ok(());
        }

        let parts: Vec<&str> = clean
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        let mut current = String::new();

        for part in parts {
            current.push('/');
            current.push_str(part);

            let encoded: String = form_urlencoded::byte_serialize(current.as_bytes()).collect();
            let url = format!("{}?path={}", YA_RESOURCE_API, encoded);

            let res = retry_request("yandex_ensure_dir", || {
                self.client
                    .put(&url)
                    .header("Authorization", self.auth_header())
                    .send()
            })
            .await
            .with_context(|| {
                format!(
                    "Failed to create Yandex Disk directory segment '{}'",
                    current
                )
            })?;

            let status = res.status();
            if status != StatusCode::CREATED
                && status != StatusCode::OK
                && status != StatusCode::CONFLICT
            {
                let err_text = res.text().await.unwrap_or_default();
                bail!(
                    "Failed to create Yandex Disk directory '{}': HTTP {} - {}",
                    current,
                    status,
                    err_text
                );
            }
        }

        Ok(())
    }

    async fn get_file_info(&self, remote_path: &str) -> Result<Option<RemoteFileInfo>> {
        let encoded: String = form_urlencoded::byte_serialize(remote_path.as_bytes()).collect();
        let url = format!("{}?path={}", YA_RESOURCE_API, encoded);

        let res = retry_request("yandex_get_info", || {
            self.client
                .get(&url)
                .header("Authorization", self.auth_header())
                .send()
        })
        .await
        .with_context(|| format!("Failed to get info for '{}'", remote_path))?;

        if res.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !res.status().is_success() {
            let status = res.status();
            let err_text = res.text().await.unwrap_or_default();
            bail!(
                "Yandex Disk get_file_info failed: HTTP {} - {}",
                status,
                err_text
            );
        }

        let ya_res: YaResourceResponse = res
            .json()
            .await
            .context("Failed to parse Yandex Disk resource JSON")?;
        let is_dir = ya_res.item_type.as_deref() == Some("dir");

        let last_modified = ya_res
            .modified
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc));

        Ok(Some(RemoteFileInfo {
            name: ya_res
                .name
                .unwrap_or_else(|| remote_path.rsplit('/').next().unwrap_or("").to_string()),
            path: ya_res.path.unwrap_or_else(|| remote_path.to_string()),
            is_dir,
            size: ya_res.size.unwrap_or(0),
            md5: ya_res.md5,
            etag: None,
            last_modified,
        }))
    }

    async fn upload_file(&self, local_path: &Path, remote_path: &str) -> Result<()> {
        let file = File::open(local_path).await.with_context(|| {
            format!(
                "Failed to open local file for upload: {}",
                local_path.display()
            )
        })?;
        let metadata = file.metadata().await?;
        let file_len = metadata.len();

        let encoded: String = form_urlencoded::byte_serialize(remote_path.as_bytes()).collect();
        let upload_url_req = format!("{}?path={}&overwrite=true", YA_UPLOAD_API, encoded);

        let res = retry_request("yandex_get_upload_url", || {
            self.client
                .get(&upload_url_req)
                .header("Authorization", self.auth_header())
                .send()
        })
        .await
        .with_context(|| format!("Failed to get upload URL for '{}'", remote_path))?;

        if !res.status().is_success() {
            let status = res.status();
            let err_text = res.text().await.unwrap_or_default();
            bail!(
                "Failed to get upload URL for '{}': HTTP {} - {}",
                remote_path,
                status,
                err_text
            );
        }

        let api_resp: YaApiResponse = res.json().await.context("Invalid upload API response")?;
        let upload_href = api_resp.href.ok_or_else(|| {
            anyhow::anyhow!("Missing href in upload response: {}", api_resp.message)
        })?;

        let upload_res = self
            .client
            .put(&upload_href)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", file_len)
            .body(file)
            .send()
            .await
            .with_context(|| {
                format!(
                    "Failed to upload stream to Yandex Disk for '{}'",
                    remote_path
                )
            })?;

        let status = upload_res.status();
        if status != StatusCode::CREATED
            && status != StatusCode::OK
            && status != StatusCode::ACCEPTED
        {
            let err_text = upload_res.text().await.unwrap_or_default();
            bail!(
                "Upload to Yandex Disk failed for '{}': HTTP {} - {}",
                remote_path,
                status,
                err_text
            );
        }

        Ok(())
    }

    async fn delete_file(&self, remote_path: &str) -> Result<()> {
        let encoded: String = form_urlencoded::byte_serialize(remote_path.as_bytes()).collect();
        let url = format!("{}?path={}&permanently=true", YA_RESOURCE_API, encoded);

        let res = retry_request("yandex_delete", || {
            self.client
                .delete(&url)
                .header("Authorization", self.auth_header())
                .send()
        })
        .await
        .with_context(|| format!("Failed to delete '{}'", remote_path))?;

        let status = res.status();
        if status != StatusCode::OK
            && status != StatusCode::NO_CONTENT
            && status != StatusCode::ACCEPTED
            && status != StatusCode::NOT_FOUND
        {
            let err_text = res.text().await.unwrap_or_default();
            bail!(
                "Delete failed for '{}': HTTP {} - {}",
                remote_path,
                status,
                err_text
            );
        }

        Ok(())
    }

    async fn read_text_file(&self, remote_path: &str) -> Result<Vec<String>> {
        let encoded: String = form_urlencoded::byte_serialize(remote_path.as_bytes()).collect();
        let url = format!("{}?path={}", YA_DOWNLOAD_API, encoded);

        let res = retry_request("yandex_get_download_url", || {
            self.client
                .get(&url)
                .header("Authorization", self.auth_header())
                .send()
        })
        .await
        .with_context(|| format!("Failed to get download URL for '{}'", remote_path))?;

        if res.status() == StatusCode::NOT_FOUND {
            bail!("File '{}' not found on Yandex Disk", remote_path);
        }

        if !res.status().is_success() {
            let status = res.status();
            let err_text = res.text().await.unwrap_or_default();
            bail!(
                "Download API returned HTTP {} for '{}': {}",
                status,
                remote_path,
                err_text
            );
        }

        let dl_resp: YaApiResponse = res
            .json()
            .await
            .context("Failed to parse download URL JSON")?;
        let dl_href = dl_resp
            .href
            .ok_or_else(|| anyhow::anyhow!("Missing download href for '{}'", remote_path))?;

        let file_res =
            retry_request("yandex_fetch_text", || self.client.get(&dl_href).send()).await?;
        let text = file_res
            .text()
            .await
            .context("Failed to read text file content")?;
        Ok(text
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect())
    }

    async fn list_dir(&self, remote_dir: &str) -> Result<Vec<RemoteFileInfo>> {
        let encoded: String = form_urlencoded::byte_serialize(remote_dir.as_bytes()).collect();
        let url = format!("{}?path={}&limit=1000", YA_RESOURCE_API, encoded);

        let res = retry_request("yandex_list_dir", || {
            self.client
                .get(&url)
                .header("Authorization", self.auth_header())
                .send()
        })
        .await
        .with_context(|| format!("Failed to list directory '{}'", remote_dir))?;

        if res.status() == StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }

        if !res.status().is_success() {
            let status = res.status();
            let err_text = res.text().await.unwrap_or_default();
            bail!(
                "Failed to list Yandex Disk directory '{}': HTTP {} - {}",
                remote_dir,
                status,
                err_text
            );
        }

        let ya_res: YaResourceResponse = res
            .json()
            .await
            .context("Failed to parse Yandex Disk list JSON")?;
        Ok(ya_res
            ._embedded
            .map(|embedded| {
                embedded
                    .items
                    .into_iter()
                    .map(|item| {
                        let last_modified = item
                            .modified
                            .as_deref()
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                            .map(|dt| dt.with_timezone(&chrono::Utc));
                        RemoteFileInfo {
                            is_dir: item.item_type == "dir",
                            name: item.name,
                            path: item.path,
                            size: item.size,
                            md5: item.md5,
                            etag: None,
                            last_modified,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
}
