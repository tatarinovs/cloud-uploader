//! Google Drive API v3 with resumable uploads.

use super::{as_md5, parse_rfc3339, RemoteFileInfo, StorageProvider};
use crate::client::{
    backoff, client_builder, ensure_success, http_error, send, upload_timeout, Failure,
    API_TIMEOUT, MAX_ATTEMPTS,
};
use crate::utils::path::{normalize_remote_dir, split_remote};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, LOCATION, RANGE};
use reqwest::{redirect, Body, Client, Response, StatusCode};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::AsyncSeekExt;
use tokio::sync::Mutex;
use tracing::{debug, warn};

const FILES_API: &str = "https://www.googleapis.com/drive/v3/files";
const UPLOAD_API: &str = "https://www.googleapis.com/upload/drive/v3/files";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const FILE_FIELDS: &str = "id,name,size,mimeType,md5Checksum,modifiedTime";

pub enum GoogleAuth {
    /// A short-lived access token (valid for about an hour).
    AccessToken(String),
    /// OAuth client credentials plus a refresh token: access tokens are renewed automatically.
    RefreshToken {
        client_id: String,
        client_secret: String,
        refresh_token: String,
    },
}

struct CachedToken {
    value: String,
    expires_at: Instant,
}

pub struct GoogleDriveProvider {
    client: Client,
    auth: GoogleAuth,
    token: Mutex<Option<CachedToken>>,
    /// Folder path -> id. The lock also serializes folder creation so that
    /// parallel uploads never create duplicate folders.
    folders: Mutex<HashMap<String, String>>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct FileList {
    #[serde(default)]
    files: Vec<DriveFile>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct DriveFile {
    id: String,
    name: String,
    size: Option<String>,
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
    #[serde(rename = "md5Checksum")]
    md5_checksum: Option<String>,
    #[serde(rename = "modifiedTime")]
    modified_time: Option<String>,
}

impl DriveFile {
    fn is_folder(&self) -> bool {
        self.mime_type.as_deref() == Some(FOLDER_MIME)
    }

    fn into_info(self, path: String) -> RemoteFileInfo {
        RemoteFileInfo {
            is_dir: self.is_folder(),
            size: self
                .size
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            md5: self.md5_checksum.as_deref().and_then(as_md5),
            modified: self.modified_time.as_deref().and_then(parse_rfc3339),
            name: self.name,
            path,
        }
    }
}

enum UploadState {
    Done,
    /// Server has persisted bytes up to (excluding) this offset.
    Incomplete(u64),
}

/// Escapes a value for use inside single quotes in a Drive `q` expression.
fn escape_query(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

impl GoogleDriveProvider {
    pub fn new(auth: GoogleAuth) -> Result<Self> {
        // 308 is "Resume Incomplete" for resumable uploads, not a redirect.
        let client = client_builder()
            .redirect(redirect::Policy::none())
            .build()
            .context("Failed to initialize HTTP client")?;
        Ok(Self {
            client,
            auth,
            token: Mutex::new(None),
            folders: Mutex::new(HashMap::new()),
        })
    }

    async fn access_token(&self) -> Result<String> {
        let (client_id, client_secret, refresh_token) = match &self.auth {
            GoogleAuth::AccessToken(token) => return Ok(token.clone()),
            GoogleAuth::RefreshToken {
                client_id,
                client_secret,
                refresh_token,
            } => (client_id, client_secret, refresh_token),
        };

        let mut cached = self.token.lock().await;
        if let Some(token) = cached.as_ref() {
            if token.expires_at > Instant::now() + Duration::from_secs(300) {
                return Ok(token.value.clone());
            }
        }
        let resp = send("google: refresh access token", || {
            Ok(self
                .client
                .post(TOKEN_URL)
                .form(&[
                    ("client_id", client_id.as_str()),
                    ("client_secret", client_secret.as_str()),
                    ("refresh_token", refresh_token.as_str()),
                    ("grant_type", "refresh_token"),
                ])
                .timeout(API_TIMEOUT))
        })
        .await?;
        let token: TokenResponse = ensure_success(resp, "Failed to refresh Google access token")
            .await?
            .json()
            .await
            .context("Invalid OAuth token response")?;
        debug!("Obtained a new Google access token");
        *cached = Some(CachedToken {
            value: token.access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(token.expires_in.unwrap_or(3600)),
        });
        Ok(token.access_token)
    }

    async fn query_files(&self, q: &str, limit: Option<usize>) -> Result<Vec<DriveFile>> {
        let token = self.access_token().await?;
        let fields = format!("nextPageToken,files({FILE_FIELDS})");
        let page_size = limit.unwrap_or(1000).min(1000).to_string();
        let mut files = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let resp = send("google: list files", || {
                let mut req = self
                    .client
                    .get(FILES_API)
                    .bearer_auth(&token)
                    .timeout(API_TIMEOUT)
                    .query(&[
                        ("q", q),
                        ("fields", fields.as_str()),
                        ("pageSize", page_size.as_str()),
                        ("spaces", "drive"),
                    ]);
                if let Some(t) = page_token.as_deref() {
                    req = req.query(&[("pageToken", t)]);
                }
                Ok(req)
            })
            .await?;
            let page: FileList = ensure_success(resp, "Google Drive query failed")
                .await?
                .json()
                .await
                .context("Invalid file list response")?;
            files.extend(page.files);
            if limit.is_some_and(|l| files.len() >= l) {
                break;
            }
            match page.next_page_token {
                Some(next) => page_token = Some(next),
                None => break,
            }
        }
        Ok(files)
    }

    /// Finds a direct child by name. `folder` restricts the kind of item.
    async fn find_child(
        &self,
        parent_id: &str,
        name: &str,
        folder: Option<bool>,
    ) -> Result<Option<DriveFile>> {
        let mut q = format!(
            "'{}' in parents and name = '{}' and trashed = false",
            escape_query(parent_id),
            escape_query(name)
        );
        match folder {
            Some(true) => q.push_str(&format!(" and mimeType = '{FOLDER_MIME}'")),
            Some(false) => q.push_str(&format!(" and mimeType != '{FOLDER_MIME}'")),
            None => {}
        }
        Ok(self.query_files(&q, Some(1)).await?.into_iter().next())
    }

    /// Resolves a folder path to its id, optionally creating missing folders.
    async fn folder_id(&self, dir: &str, create: bool) -> Result<Option<String>> {
        let dir = normalize_remote_dir(dir);
        if dir == "/" {
            return Ok(Some("root".to_string()));
        }
        let mut cache = self.folders.lock().await;
        if let Some(id) = cache.get(&dir) {
            return Ok(Some(id.clone()));
        }

        let mut parent = "root".to_string();
        let mut current = String::new();
        for segment in dir.split('/').filter(|s| !s.is_empty()) {
            current.push('/');
            current.push_str(segment);
            if let Some(id) = cache.get(&current) {
                parent = id.clone();
                continue;
            }
            let id = match self.find_child(&parent, segment, Some(true)).await? {
                Some(folder) => folder.id,
                None if create => self.create_folder(segment, &parent).await?,
                None => return Ok(None),
            };
            cache.insert(current.clone(), id.clone());
            parent = id;
        }
        Ok(Some(parent))
    }

    async fn create_folder(&self, name: &str, parent_id: &str) -> Result<String> {
        let token = self.access_token().await?;
        let metadata = json!({ "name": name, "mimeType": FOLDER_MIME, "parents": [parent_id] });
        let resp = send("google: create folder", || {
            Ok(self
                .client
                .post(FILES_API)
                .bearer_auth(&token)
                .query(&[("fields", "id")])
                .json(&metadata)
                .timeout(API_TIMEOUT))
        })
        .await?;
        #[derive(Deserialize)]
        struct Created {
            id: String,
        }
        let created: Created = ensure_success(resp, format!("Failed to create folder '{name}'"))
            .await?
            .json()
            .await
            .context("Invalid create folder response")?;
        Ok(created.id)
    }

    /// Locates an item by path without creating anything.
    async fn find_item(
        &self,
        remote_path: &str,
        folder: Option<bool>,
    ) -> Result<Option<DriveFile>> {
        let (parent, name) = split_remote(remote_path);
        if name.is_empty() {
            return Ok(None);
        }
        match self.folder_id(&parent, false).await? {
            Some(parent_id) => self.find_child(&parent_id, &name, folder).await,
            None => Ok(None),
        }
    }

    async fn start_session(
        &self,
        existing_id: Option<&str>,
        parent_id: &str,
        name: &str,
        size: u64,
    ) -> Result<String> {
        let token = self.access_token().await?;
        let resp = send("google: start upload session", || {
            let req = match existing_id {
                Some(id) => self
                    .client
                    .patch(format!("{UPLOAD_API}/{id}"))
                    .json(&json!({})),
                None => self
                    .client
                    .post(UPLOAD_API)
                    .json(&json!({ "name": name, "parents": [parent_id] })),
            };
            Ok(req
                .bearer_auth(&token)
                .query(&[("uploadType", "resumable")])
                .header("X-Upload-Content-Type", "application/octet-stream")
                .header("X-Upload-Content-Length", size)
                .timeout(API_TIMEOUT))
        })
        .await?;
        let resp = ensure_success(resp, format!("Failed to start upload of '{name}'")).await?;
        resp.headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .context("Google Drive did not return an upload session URL")
    }

    /// Sends the file from `offset` to the end into the upload session.
    async fn put_from(
        &self,
        session: &str,
        local: &Path,
        offset: u64,
        size: u64,
    ) -> Result<UploadState, Failure> {
        let mut file = tokio::fs::File::open(local)
            .await
            .with_context(|| format!("Failed to open '{}'", local.display()))?;
        file.seek(SeekFrom::Start(offset)).await?;
        let remaining = size - offset;
        let mut req = self
            .client
            .put(session)
            .header(CONTENT_LENGTH, remaining)
            .timeout(upload_timeout(remaining));
        if size > 0 {
            req = req.header(CONTENT_RANGE, format!("bytes {offset}-{}/{size}", size - 1));
        }
        let resp = req.body(Body::from(file)).send().await?;
        classify_upload_response(resp).await
    }

    /// Asks the server how much of the upload it has already received.
    async fn query_offset(&self, session: &str, size: u64) -> Result<UploadState> {
        let resp = send("google: query upload status", || {
            Ok(self
                .client
                .put(session)
                .header(CONTENT_LENGTH, 0)
                .header(CONTENT_RANGE, format!("bytes */{size}"))
                .timeout(API_TIMEOUT))
        })
        .await?;
        classify_upload_response(resp)
            .await
            .map_err(Failure::into_inner)
    }

    async fn upload_resumable(&self, session: &str, local: &Path, size: u64) -> Result<()> {
        let mut offset = 0;
        let mut attempt = 1;
        loop {
            match self.put_from(session, local, offset, size).await {
                Ok(UploadState::Done) => return Ok(()),
                Ok(UploadState::Incomplete(next)) => {
                    if next <= offset {
                        attempt += 1;
                        if attempt > MAX_ATTEMPTS {
                            bail!("Upload is not making progress at byte {offset}");
                        }
                    }
                    offset = next;
                }
                Err(Failure::Fatal(err)) => return Err(err),
                Err(Failure::Transient(err)) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(err);
                    }
                    let delay = backoff(attempt);
                    warn!(
                        "google: upload interrupted: {err:#}; resuming in {:.1}s (attempt {attempt}/{MAX_ATTEMPTS})",
                        delay.as_secs_f32()
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    match self.query_offset(session, size).await? {
                        UploadState::Done => return Ok(()),
                        UploadState::Incomplete(next) => {
                            debug!("google: resuming upload at byte {next} of {size}");
                            offset = next;
                        }
                    }
                }
            }
        }
    }
}

async fn classify_upload_response(resp: Response) -> Result<UploadState, Failure> {
    match resp.status().as_u16() {
        200 | 201 => Ok(UploadState::Done),
        308 => {
            // "Range: bytes=0-12345" lists what the server has persisted.
            let next = resp
                .headers()
                .get(RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|r| r.rsplit('-').next())
                .and_then(|end| end.trim().parse::<u64>().ok())
                .map_or(0, |end| end + 1);
            Ok(UploadState::Incomplete(next))
        }
        404 | 410 => Err(Failure::Fatal(anyhow!(
            "Google Drive upload session expired"
        ))),
        _ => Err(Failure::from_response(resp, "Google Drive upload failed").await),
    }
}

#[async_trait]
impl StorageProvider for GoogleDriveProvider {
    fn name(&self) -> &'static str {
        "Google Drive"
    }

    async fn ensure_dir(&self, remote_dir: &str) -> Result<()> {
        self.folder_id(remote_dir, true).await?;
        Ok(())
    }

    async fn get_file_info(&self, remote_path: &str) -> Result<Option<RemoteFileInfo>> {
        let path = normalize_remote_dir(remote_path);
        Ok(self
            .find_item(&path, None)
            .await?
            .map(|file| file.into_info(path)))
    }

    async fn upload_file(&self, local_path: &Path, remote_path: &str) -> Result<()> {
        let (parent, name) = split_remote(remote_path);
        if name.is_empty() {
            bail!("Invalid remote file path '{remote_path}'");
        }
        let parent_id = self
            .folder_id(&parent, true)
            .await?
            .with_context(|| format!("Failed to resolve folder '{parent}'"))?;
        let existing = self.find_child(&parent_id, &name, Some(false)).await?;
        let size = tokio::fs::metadata(local_path)
            .await
            .with_context(|| format!("Failed to read metadata of '{}'", local_path.display()))?
            .len();

        let session = self
            .start_session(
                existing.as_ref().map(|f| f.id.as_str()),
                &parent_id,
                &name,
                size,
            )
            .await?;
        self.upload_resumable(&session, local_path, size).await
    }

    async fn delete(&self, remote_path: &str, is_dir: bool) -> Result<()> {
        let path = normalize_remote_dir(remote_path);
        if path == "/" {
            bail!("Refusing to delete the root folder");
        }
        let Some(item) = self.find_item(&path, Some(is_dir)).await? else {
            return Ok(());
        };
        let token = self.access_token().await?;
        let url = format!("{FILES_API}/{}", item.id);
        let resp = send("google: delete", || {
            Ok(self
                .client
                .delete(&url)
                .bearer_auth(&token)
                .timeout(API_TIMEOUT))
        })
        .await?;
        if !resp.status().is_success() && resp.status() != StatusCode::NOT_FOUND {
            return Err(http_error(resp, format!("Failed to delete '{path}'")).await);
        }
        if is_dir {
            let prefix = format!("{path}/");
            self.folders
                .lock()
                .await
                .retain(|cached, _| cached != &path && !cached.starts_with(&prefix));
        }
        Ok(())
    }

    async fn read_text_file(&self, remote_path: &str) -> Result<Option<String>> {
        let path = normalize_remote_dir(remote_path);
        let Some(item) = self.find_item(&path, Some(false)).await? else {
            return Ok(None);
        };
        let token = self.access_token().await?;
        let url = format!("{FILES_API}/{}", item.id);
        let resp = send("google: download", || {
            Ok(self
                .client
                .get(&url)
                .bearer_auth(&token)
                .query(&[("alt", "media")])
                .timeout(API_TIMEOUT))
        })
        .await?;
        let text = ensure_success(resp, format!("Failed to download '{path}'"))
            .await?
            .text()
            .await?;
        Ok(Some(text))
    }

    async fn list_dir(&self, remote_dir: &str) -> Result<Vec<RemoteFileInfo>> {
        let dir = normalize_remote_dir(remote_dir);
        let Some(folder_id) = self.folder_id(&dir, false).await? else {
            return Ok(Vec::new());
        };
        let q = format!(
            "'{}' in parents and trashed = false",
            escape_query(&folder_id)
        );
        Ok(self
            .query_files(&q, None)
            .await?
            .into_iter()
            .map(|file| {
                let path = crate::utils::path::join_remote(&dir, &file.name);
                file.into_info(path)
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_query_values() {
        assert_eq!(escape_query("O'Brien.txt"), "O\\'Brien.txt");
        assert_eq!(escape_query("a\\b"), "a\\\\b");
    }
}
