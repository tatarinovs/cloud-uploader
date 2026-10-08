//! Yandex Disk REST API: https://yandex.ru/dev/disk-api/doc/

use super::{as_md5, parse_rfc3339, RemoteFileInfo, StorageProvider};
use crate::client::{
    build_http_client, ensure_success, file_body, http_error, retry_transient, send,
    upload_timeout, Failure, API_TIMEOUT,
};
use crate::utils::path::normalize_remote_dir;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH};
use reqwest::{Client, Method, RequestBuilder, StatusCode};
use serde::Deserialize;
use std::path::Path;
use std::time::{Duration, Instant};

const API: &str = "https://cloud-api.yandex.net/v1/disk/resources";
const PAGE_SIZE: usize = 1000;
const ITEM_FIELDS: &str = "name,path,type,size,md5,modified";
const LIST_FIELDS: &str = "_embedded.items.name,_embedded.items.path,_embedded.items.type,\
                           _embedded.items.size,_embedded.items.md5,_embedded.items.modified";
const OPERATION_TIMEOUT: Duration = Duration::from_secs(600);

pub struct YandexDiskProvider {
    auth: String,
    client: Client,
}

#[derive(Deserialize)]
struct Link {
    href: String,
}

#[derive(Deserialize)]
struct Resource {
    name: String,
    path: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    size: u64,
    md5: Option<String>,
    modified: Option<String>,
}

impl Resource {
    fn into_info(self) -> RemoteFileInfo {
        let path = self.path.strip_prefix("disk:").unwrap_or(&self.path);
        RemoteFileInfo {
            path: normalize_remote_dir(path),
            is_dir: self.kind == "dir",
            size: self.size,
            md5: self.md5.as_deref().and_then(as_md5),
            modified: self.modified.as_deref().and_then(parse_rfc3339),
            name: self.name,
        }
    }
}

#[derive(Deserialize)]
struct Listing {
    #[serde(rename = "_embedded")]
    embedded: Option<ListingItems>,
}

#[derive(Deserialize)]
struct ListingItems {
    #[serde(default)]
    items: Vec<Resource>,
}

#[derive(Deserialize)]
struct Operation {
    status: String,
}

impl YandexDiskProvider {
    pub fn new(token: &str) -> Result<Self> {
        Ok(Self {
            auth: format!("OAuth {token}"),
            client: build_http_client()?,
        })
    }

    fn api(&self, method: Method, endpoint: &str) -> RequestBuilder {
        self.client
            .request(method, format!("{API}{endpoint}"))
            .header(AUTHORIZATION, &self.auth)
            .timeout(API_TIMEOUT)
    }

    /// Requests a fresh upload URL and streams the file to it.
    async fn upload_once(&self, local: &Path, remote: &str) -> Result<(), Failure> {
        let resp = send("yandex: request upload URL", || {
            Ok(self
                .api(Method::GET, "/upload")
                .query(&[("path", remote), ("overwrite", "true")]))
        })
        .await?;
        if !resp.status().is_success() {
            return Err(Failure::from_response(
                resp,
                format!("Failed to get upload URL for '{remote}'"),
            )
            .await);
        }
        let link: Link = resp.json().await.context("Invalid upload URL response")?;

        let (body, len) = file_body(local)?;
        let resp = self
            .client
            .put(&link.href)
            .header(CONTENT_LENGTH, len)
            .timeout(upload_timeout(len))
            .body(body)
            .send()
            .await?;

        match resp.status() {
            StatusCode::CREATED | StatusCode::OK | StatusCode::ACCEPTED => Ok(()),
            _ => Err(Failure::from_response(resp, format!("Upload of '{remote}' failed")).await),
        }
    }

    async fn wait_for_operation(&self, href: &str) -> Result<()> {
        let started = Instant::now();
        loop {
            let resp = send("yandex: operation status", || {
                Ok(self
                    .client
                    .get(href)
                    .header(AUTHORIZATION, &self.auth)
                    .timeout(API_TIMEOUT))
            })
            .await?;
            let op: Operation = ensure_success(resp, "Failed to query operation status")
                .await?
                .json()
                .await
                .context("Invalid operation status response")?;
            match op.status.as_str() {
                "success" => return Ok(()),
                "failed" => bail!("Yandex Disk reported that the operation failed"),
                _ if started.elapsed() > OPERATION_TIMEOUT => {
                    bail!("Timed out waiting for a Yandex Disk operation to finish")
                }
                _ => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
    }
}

#[async_trait]
impl StorageProvider for YandexDiskProvider {
    fn name(&self) -> &'static str {
        "Yandex Disk"
    }

    async fn ensure_dir(&self, remote_dir: &str) -> Result<()> {
        let dir = normalize_remote_dir(remote_dir);
        if dir == "/" {
            return Ok(());
        }
        match self.get_file_info(&dir).await? {
            Some(info) if info.is_dir => return Ok(()),
            Some(_) => bail!("'{dir}' exists on Yandex Disk but is a file"),
            None => {}
        }

        let mut current = String::new();
        for segment in dir.split('/').filter(|s| !s.is_empty()) {
            current.push('/');
            current.push_str(segment);
            let resp = send("yandex: create folder", || {
                Ok(self
                    .api(Method::PUT, "")
                    .query(&[("path", current.as_str())]))
            })
            .await?;
            // 409 means the folder already exists (we create parents first).
            if !matches!(
                resp.status(),
                StatusCode::CREATED | StatusCode::OK | StatusCode::CONFLICT
            ) {
                return Err(http_error(resp, format!("Failed to create folder '{current}'")).await);
            }
        }
        Ok(())
    }

    async fn get_file_info(&self, remote_path: &str) -> Result<Option<RemoteFileInfo>> {
        let path = normalize_remote_dir(remote_path);
        let resp = send("yandex: get info", || {
            Ok(self
                .api(Method::GET, "")
                .query(&[("path", path.as_str()), ("fields", ITEM_FIELDS)]))
        })
        .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resource: Resource = ensure_success(resp, format!("Failed to get info for '{path}'"))
            .await?
            .json()
            .await
            .context("Invalid resource response from Yandex Disk")?;
        Ok(Some(resource.into_info()))
    }

    async fn upload_file(&self, local_path: &Path, remote_path: &str) -> Result<()> {
        let path = normalize_remote_dir(remote_path);
        retry_transient(&format!("yandex: upload '{path}'"), || {
            self.upload_once(local_path, &path)
        })
        .await
    }

    async fn delete(&self, remote_path: &str, _is_dir: bool) -> Result<()> {
        let path = normalize_remote_dir(remote_path);
        if path == "/" {
            bail!("Refusing to delete the root folder");
        }
        let resp = send("yandex: delete", || {
            Ok(self
                .api(Method::DELETE, "")
                .query(&[("path", path.as_str()), ("permanently", "true")]))
        })
        .await?;
        match resp.status() {
            StatusCode::NO_CONTENT | StatusCode::OK | StatusCode::NOT_FOUND => Ok(()),
            StatusCode::ACCEPTED => {
                // Large folders are deleted asynchronously.
                let link: Link = resp.json().await.context("Invalid delete response")?;
                self.wait_for_operation(&link.href).await
            }
            _ => Err(http_error(resp, format!("Failed to delete '{path}'")).await),
        }
    }

    async fn read_text_file(&self, remote_path: &str) -> Result<Option<String>> {
        let path = normalize_remote_dir(remote_path);
        let resp = send("yandex: request download URL", || {
            Ok(self
                .api(Method::GET, "/download")
                .query(&[("path", path.as_str())]))
        })
        .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let link: Link = ensure_success(resp, format!("Failed to get download URL for '{path}'"))
            .await?
            .json()
            .await
            .context("Invalid download URL response")?;

        let resp = send("yandex: download", || {
            Ok(self.client.get(&link.href).timeout(API_TIMEOUT))
        })
        .await?;
        let text = ensure_success(resp, format!("Failed to download '{path}'"))
            .await?
            .text()
            .await
            .with_context(|| format!("Failed to read '{path}'"))?;
        Ok(Some(text))
    }

    async fn list_dir(&self, remote_dir: &str) -> Result<Vec<RemoteFileInfo>> {
        let dir = normalize_remote_dir(remote_dir);
        let limit = PAGE_SIZE.to_string();
        let mut items = Vec::new();
        loop {
            let offset = items.len().to_string();
            let resp = send("yandex: list folder", || {
                Ok(self.api(Method::GET, "").query(&[
                    ("path", dir.as_str()),
                    ("limit", limit.as_str()),
                    ("offset", offset.as_str()),
                    ("fields", LIST_FIELDS),
                ]))
            })
            .await?;
            if resp.status() == StatusCode::NOT_FOUND {
                break;
            }
            let listing: Listing = ensure_success(resp, format!("Failed to list '{dir}'"))
                .await?
                .json()
                .await
                .context("Invalid folder listing from Yandex Disk")?;
            let page = listing.embedded.map(|e| e.items).unwrap_or_default();
            let count = page.len();
            items.extend(page.into_iter().map(Resource::into_info));
            if count < PAGE_SIZE {
                break;
            }
        }
        Ok(items)
    }
}
