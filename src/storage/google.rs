use crate::client::{build_http_client, retry_request};
use crate::storage::{RemoteFileInfo, StorageProvider};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::sync::RwLock;

const GDRIVE_FILES_API: &str = "https://www.googleapis.com/drive/v3/files";
const GDRIVE_UPLOAD_API: &str = "https://www.googleapis.com/upload/drive/v3/files";

pub struct GoogleDriveProvider {
    token: String,
    client: Client,
    folder_cache: Arc<RwLock<HashMap<String, String>>>,
}

impl GoogleDriveProvider {
    pub fn new(token: String) -> Result<Self> {
        let client = build_http_client(Some(Duration::from_secs(600)))?;
        let mut cache = HashMap::new();
        cache.insert("".to_string(), "root".to_string());
        cache.insert("/".to_string(), "root".to_string());

        Ok(Self {
            token,
            client,
            folder_cache: Arc::new(RwLock::new(cache)),
        })
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.token)
    }

    async fn resolve_or_create_folder(&self, path: &str) -> Result<String> {
        let clean = path.trim().trim_matches('/');
        if clean.is_empty() {
            return Ok("root".to_string());
        }

        {
            let cache = self.folder_cache.read().await;
            if let Some(id) = cache.get(clean) {
                return Ok(id.clone());
            }
        }

        let segments: Vec<&str> = clean.split('/').filter(|s| !s.is_empty()).collect();
        let mut current_parent = "root".to_string();
        let mut current_path = String::new();

        for seg in segments {
            if !current_path.is_empty() {
                current_path.push('/');
            }
            current_path.push_str(seg);

            {
                let cache = self.folder_cache.read().await;
                if let Some(id) = cache.get(&current_path) {
                    current_parent = id.clone();
                    continue;
                }
            }

            // Search if folder already exists under current_parent
            let q = format!(
                "'{}' in parents and name = '{}' and mimeType = 'application/vnd.google-apps.folder' and trashed = false",
                current_parent, seg
            );
            let search_url = format!(
                "{}?q={}&fields=files(id,name)",
                GDRIVE_FILES_API,
                urlencoding_encode(&q)
            );

            let res = retry_request("gdrive_search_folder", || {
                self.client
                    .get(&search_url)
                    .header(AUTHORIZATION, self.auth_header())
                    .send()
            })
            .await
            .with_context(|| format!("Failed to search Google Drive folder '{}'", seg))?;

            if !res.status().is_success() {
                let status = res.status();
                let err_text = res.text().await.unwrap_or_default();
                bail!(
                    "Google Drive folder search failed: HTTP {} - {}",
                    status,
                    err_text
                );
            }

            let list_resp: GDriveFileList =
                res.json().await.context("Invalid folder search JSON")?;

            let folder_id = if let Some(first) = list_resp.files.into_iter().next() {
                first.id
            } else {
                // Create folder
                let create_payload = GDriveCreateFolder {
                    name: seg.to_string(),
                    mime_type: "application/vnd.google-apps.folder".to_string(),
                    parents: vec![current_parent.clone()],
                };

                let create_res = retry_request("gdrive_create_folder", || {
                    self.client
                        .post(GDRIVE_FILES_API)
                        .header(AUTHORIZATION, self.auth_header())
                        .json(&create_payload)
                        .send()
                })
                .await
                .with_context(|| format!("Failed to create folder '{}'", seg))?;

                if !create_res.status().is_success() {
                    let status = create_res.status();
                    let err_text = create_res.text().await.unwrap_or_default();
                    bail!(
                        "Failed to create Google Drive folder '{}': HTTP {} - {}",
                        seg,
                        status,
                        err_text
                    );
                }

                let created_item: GDriveFileItem = create_res
                    .json()
                    .await
                    .context("Invalid created folder JSON")?;
                created_item.id
            };

            let mut cache = self.folder_cache.write().await;
            cache.insert(current_path.clone(), folder_id.clone());
            current_parent = folder_id;
        }

        Ok(current_parent)
    }

    async fn find_file_in_folder(
        &self,
        parent_id: &str,
        file_name: &str,
    ) -> Result<Option<GDriveFileItem>> {
        let q = format!(
            "'{}' in parents and name = '{}' and trashed = false",
            parent_id, file_name
        );
        let url = format!(
            "{}?q={}&fields=files(id,name,size,mimeType,md5Checksum,parents)&pageSize=1",
            GDRIVE_FILES_API,
            urlencoding_encode(&q)
        );

        let res = retry_request("gdrive_find_file", || {
            self.client
                .get(&url)
                .header(AUTHORIZATION, self.auth_header())
                .send()
        })
        .await
        .with_context(|| format!("Failed to find file '{}'", file_name))?;

        if !res.status().is_success() {
            let status = res.status();
            let err_text = res.text().await.unwrap_or_default();
            bail!(
                "Google Drive file search failed: HTTP {} - {}",
                status,
                err_text
            );
        }

        let list_resp: GDriveFileList = res.json().await.context("Invalid file search response")?;
        Ok(list_resp.files.into_iter().next())
    }
}

#[derive(Serialize)]
struct GDriveCreateFolder {
    name: String,
    #[serde(rename = "mimeType")]
    mime_type: String,
    parents: Vec<String>,
}

#[derive(Deserialize)]
struct GDriveFileList {
    #[serde(default)]
    files: Vec<GDriveFileItem>,
}

#[derive(Deserialize, Clone)]
pub struct GDriveFileItem {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
    #[serde(rename = "md5Checksum")]
    pub md5_checksum: Option<String>,
}

#[async_trait]
impl StorageProvider for GoogleDriveProvider {
    fn name(&self) -> &'static str {
        "Google Drive"
    }

    async fn ensure_dir(&self, remote_dir: &str) -> Result<()> {
        self.resolve_or_create_folder(remote_dir).await?;
        Ok(())
    }

    async fn get_file_info(&self, remote_path: &str) -> Result<Option<RemoteFileInfo>> {
        let clean = remote_path.trim().trim_matches('/');
        let (parent_path, file_name) = match clean.rsplit_once('/') {
            Some((p, f)) => (p, f),
            None => ("", clean),
        };

        let parent_id = match self.resolve_or_create_folder(parent_path).await {
            Ok(id) => id,
            Err(_) => return Ok(None),
        };

        let file_opt = self.find_file_in_folder(&parent_id, file_name).await?;
        match file_opt {
            Some(f) => {
                let is_dir = f.mime_type.as_deref() == Some("application/vnd.google-apps.folder");
                let size = f.size.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
                Ok(Some(RemoteFileInfo {
                    name: f.name,
                    path: format!("/{}", clean),
                    is_dir,
                    size,
                    md5: f.md5_checksum,
                    etag: None,
                }))
            }
            None => Ok(None),
        }
    }

    async fn upload_file(&self, local_path: &Path, remote_path: &str) -> Result<()> {
        let clean = remote_path.trim().trim_matches('/');
        let (parent_path, file_name) = match clean.rsplit_once('/') {
            Some((p, f)) => (p, f),
            None => ("", clean),
        };

        let parent_id = self.resolve_or_create_folder(parent_path).await?;

        // Read local file
        let mut file = File::open(local_path)
            .await
            .with_context(|| format!("Failed to open file: {}", local_path.display()))?;
        let mut file_bytes = Vec::new();
        file.read_to_end(&mut file_bytes).await?;

        // Check if existing file needs updating
        let existing = self.find_file_in_folder(&parent_id, file_name).await?;

        if let Some(existing_file) = existing {
            // Update existing file content
            let url = format!(
                "{}/{}?uploadType=media",
                GDRIVE_UPLOAD_API, existing_file.id
            );
            let res = self
                .client
                .patch(&url)
                .header(AUTHORIZATION, self.auth_header())
                .header(CONTENT_TYPE, "application/octet-stream")
                .body(file_bytes)
                .send()
                .await
                .with_context(|| format!("Failed to update Google Drive file '{}'", file_name))?;

            if !res.status().is_success() {
                let status = res.status();
                let err_text = res.text().await.unwrap_or_default();
                bail!(
                    "Google Drive file update failed: HTTP {} - {}",
                    status,
                    err_text
                );
            }
        } else {
            // Create new file via multipart upload
            let boundary = "-------CloudUploaderBoundary7MA4YWxkTrZu0gW";
            let metadata = serde_json::json!({
                "name": file_name,
                "parents": [parent_id]
            });

            let mut body = Vec::new();
            body.extend_from_slice(
                format!(
                    "--{}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n",
                    boundary
                )
                .as_bytes(),
            );
            body.extend_from_slice(serde_json::to_string(&metadata)?.as_bytes());
            body.extend_from_slice(
                format!(
                    "\r\n--{}\r\nContent-Type: application/octet-stream\r\n\r\n",
                    boundary
                )
                .as_bytes(),
            );
            body.extend_from_slice(&file_bytes);
            body.extend_from_slice(format!("\r\n--{}--\r\n", boundary).as_bytes());

            let url = format!("{}?uploadType=multipart", GDRIVE_UPLOAD_API);
            let res = self
                .client
                .post(&url)
                .header(AUTHORIZATION, self.auth_header())
                .header(
                    CONTENT_TYPE,
                    format!("multipart/related; boundary={}", boundary),
                )
                .body(body)
                .send()
                .await
                .with_context(|| format!("Failed to upload Google Drive file '{}'", file_name))?;

            if !res.status().is_success() {
                let status = res.status();
                let err_text = res.text().await.unwrap_or_default();
                bail!(
                    "Google Drive file upload failed: HTTP {} - {}",
                    status,
                    err_text
                );
            }
        }

        Ok(())
    }

    async fn delete_file(&self, remote_path: &str) -> Result<()> {
        let clean = remote_path.trim().trim_matches('/');
        let (parent_path, file_name) = match clean.rsplit_once('/') {
            Some((p, f)) => (p, f),
            None => ("", clean),
        };

        let parent_id = self.resolve_or_create_folder(parent_path).await?;
        if let Some(item) = self.find_file_in_folder(&parent_id, file_name).await? {
            let url = format!("{}/{}", GDRIVE_FILES_API, item.id);
            let res = retry_request("gdrive_delete", || {
                self.client
                    .delete(&url)
                    .header(AUTHORIZATION, self.auth_header())
                    .send()
            })
            .await?;

            if !res.status().is_success() && res.status() != StatusCode::NOT_FOUND {
                let status = res.status();
                let err_text = res.text().await.unwrap_or_default();
                bail!("Google Drive delete failed: HTTP {} - {}", status, err_text);
            }
        }

        Ok(())
    }

    async fn read_text_file(&self, remote_path: &str) -> Result<Vec<String>> {
        let clean = remote_path.trim().trim_matches('/');
        let (parent_path, file_name) = match clean.rsplit_once('/') {
            Some((p, f)) => (p, f),
            None => ("", clean),
        };

        let parent_id = self.resolve_or_create_folder(parent_path).await?;
        let item = self
            .find_file_in_folder(&parent_id, file_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("File '{}' not found in Google Drive", remote_path))?;

        let url = format!("{}/{}?alt=media", GDRIVE_FILES_API, item.id);
        let res = retry_request("gdrive_download_text", || {
            self.client
                .get(&url)
                .header(AUTHORIZATION, self.auth_header())
                .send()
        })
        .await?;

        if !res.status().is_success() {
            let status = res.status();
            let err_text = res.text().await.unwrap_or_default();
            bail!(
                "Failed to download text file from Google Drive: HTTP {} - {}",
                status,
                err_text
            );
        }

        let text = res.text().await.context("Failed to decode text response")?;
        Ok(text
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect())
    }

    async fn list_dir(&self, remote_dir: &str) -> Result<Vec<RemoteFileInfo>> {
        let clean = remote_dir.trim().trim_matches('/');
        let folder_id = self.resolve_or_create_folder(clean).await?;

        let q = format!("'{}' in parents and trashed = false", folder_id);
        let url = format!(
            "{}?q={}&fields=files(id,name,size,mimeType,md5Checksum)&pageSize=1000",
            GDRIVE_FILES_API,
            urlencoding_encode(&q)
        );

        let res = retry_request("gdrive_list_dir", || {
            self.client
                .get(&url)
                .header(AUTHORIZATION, self.auth_header())
                .send()
        })
        .await
        .with_context(|| format!("Failed to list Google Drive directory '{}'", remote_dir))?;

        if !res.status().is_success() {
            let status = res.status();
            let err_text = res.text().await.unwrap_or_default();
            bail!(
                "Google Drive list_dir failed: HTTP {} - {}",
                status,
                err_text
            );
        }

        let list_resp: GDriveFileList = res.json().await.context("Invalid list JSON")?;
        let items = list_resp
            .files
            .into_iter()
            .map(|f| {
                let is_dir = f.mime_type.as_deref() == Some("application/vnd.google-apps.folder");
                let size = f.size.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
                let path = if clean.is_empty() {
                    format!("/{}", f.name)
                } else {
                    format!("/{}/{}", clean, f.name)
                };

                RemoteFileInfo {
                    name: f.name,
                    path,
                    is_dir,
                    size,
                    md5: f.md5_checksum,
                    etag: None,
                }
            })
            .collect();

        Ok(items)
    }
}

fn urlencoding_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
