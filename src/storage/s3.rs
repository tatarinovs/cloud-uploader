use crate::client::build_http_client;
use crate::storage::{RemoteFileInfo, StorageProvider};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use hmac::{Hmac, Mac};
use quick_xml::de::from_str;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

pub struct S3Provider {
    endpoint: String,
    bucket: String,
    access_key: String,
    secret_key: String,
    region: String,
    client: Client,
}

impl S3Provider {
    pub fn new(
        endpoint: String,
        bucket: String,
        access_key: String,
        secret_key: String,
        region: Option<String>,
    ) -> Result<Self> {
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            bucket,
            access_key,
            secret_key,
            region: region.unwrap_or_else(|| "auto".to_string()),
            client: build_http_client(Some(Duration::from_secs(600)))?,
        })
    }

    fn sign_request(
        &self,
        method: &str,
        path: &str,
        query: &str,
        payload_hash: &str,
    ) -> Result<(String, String)> {
        let now = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();

        let host = if let Ok(url) = url::Url::parse(&self.endpoint) {
            url.host_str().unwrap_or(&self.endpoint).to_string()
        } else {
            self.endpoint.clone()
        };

        let canonical_uri = if path.is_empty() {
            "/".to_string()
        } else if !path.starts_with('/') {
            format!("/{}", path)
        } else {
            path.to_string()
        };

        let canonical_query = query;
        let canonical_headers = format!(
            "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
            host, payload_hash, amz_date
        );
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";

        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method, canonical_uri, canonical_query, canonical_headers, signed_headers, payload_hash
        );

        let mut hasher = Sha256::new();
        hasher.update(canonical_request.as_bytes());
        let hashed_canonical_request = hex::encode(hasher.finalize());

        let credential_scope = format!("{}/{}/s3/aws4_request", date_stamp, self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date, credential_scope, hashed_canonical_request
        );

        type HmacSha256 = Hmac<Sha256>;
        let k_secret = format!("AWS4{}", self.secret_key);
        let mut mac1 = HmacSha256::new_from_slice(k_secret.as_bytes())?;
        mac1.update(date_stamp.as_bytes());
        let k_date = mac1.finalize().into_bytes();

        let mut mac2 = HmacSha256::new_from_slice(&k_date)?;
        mac2.update(self.region.as_bytes());
        let k_region = mac2.finalize().into_bytes();

        let mut mac3 = HmacSha256::new_from_slice(&k_region)?;
        mac3.update(b"s3");
        let k_service = mac3.finalize().into_bytes();

        let mut mac4 = HmacSha256::new_from_slice(&k_service)?;
        mac4.update(b"aws4_request");
        let k_signing = mac4.finalize().into_bytes();

        let mut mac5 = HmacSha256::new_from_slice(&k_signing)?;
        mac5.update(string_to_sign.as_bytes());
        let signature = hex::encode(mac5.finalize().into_bytes());

        let auth_header = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.access_key, credential_scope, signed_headers, signature
        );

        Ok((amz_date, auth_header))
    }

    fn key_for_path(&self, remote_path: &str) -> String {
        remote_path.trim_matches('/').to_string()
    }
}

#[derive(Debug, Deserialize)]
struct ListBucketResult {
    #[serde(rename = "Contents", default)]
    contents: Vec<S3Object>,
    #[serde(rename = "CommonPrefixes", default)]
    common_prefixes: Vec<S3Prefix>,
}

#[derive(Debug, Deserialize)]
struct S3Object {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "Size", default)]
    size: i64,
    #[serde(rename = "ETag", default)]
    etag: Option<String>,
}

#[derive(Debug, Deserialize)]
struct S3Prefix {
    #[serde(rename = "Prefix")]
    prefix: String,
}

#[async_trait]
impl StorageProvider for S3Provider {
    fn name(&self) -> &'static str {
        "S3 / Cloudflare R2 / MinIO"
    }

    async fn ensure_dir(&self, remote_dir: &str) -> Result<()> {
        let clean = remote_dir.trim().trim_matches('/');
        if clean.is_empty() {
            return Ok(());
        }

        let key = format!("{}/", clean);
        let path = format!("/{}/{}", self.bucket, key);
        let payload_hash = hex::encode(Sha256::digest(b""));
        let (amz_date, auth) = self.sign_request("PUT", &path, "", &payload_hash)?;

        let url = format!("{}{}", self.endpoint, path);
        let res = self
            .client
            .put(&url)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", &payload_hash)
            .header("Authorization", auth)
            .header("Content-Length", 0)
            .send()
            .await?;

        if !res.status().is_success() && res.status() != StatusCode::CONFLICT {
            let status = res.status();
            let err_text = res.text().await.unwrap_or_default();
            bail!(
                "S3 ensure_dir failed for '{}': HTTP {} - {}",
                remote_dir,
                status,
                err_text
            );
        }

        Ok(())
    }

    async fn get_file_info(&self, remote_path: &str) -> Result<Option<RemoteFileInfo>> {
        let key = self.key_for_path(remote_path);
        let path = format!("/{}/{}", self.bucket, key);
        let payload_hash = hex::encode(Sha256::digest(b""));
        let (amz_date, auth) = self.sign_request("HEAD", &path, "", &payload_hash)?;

        let url = format!("{}{}", self.endpoint, path);
        let res = self
            .client
            .head(&url)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", &payload_hash)
            .header("Authorization", auth)
            .send()
            .await?;

        if res.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !res.status().is_success() {
            let status = res.status();
            bail!(
                "S3 get_file_info failed for '{}': HTTP {}",
                remote_path,
                status
            );
        }

        let headers = res.headers();
        let size = headers
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);

        let etag = headers
            .get(reqwest::header::ETAG)
            .and_then(|h| h.to_str().ok())
            .map(|s| s.trim_matches('"').to_string());

        // In S3, if ETag is exactly 32 hex chars without '-', it is MD5
        let md5 = etag.as_ref().and_then(|e| {
            if e.len() == 32 && !e.contains('-') {
                Some(e.clone())
            } else {
                None
            }
        });

        let name = remote_path.rsplit('/').next().unwrap_or("").to_string();

        Ok(Some(RemoteFileInfo {
            name,
            path: format!("/{}", key),
            is_dir: false,
            size,
            md5,
            etag,
        }))
    }

    async fn upload_file(&self, local_path: &Path, remote_path: &str) -> Result<()> {
        let key = self.key_for_path(remote_path);
        let path = format!("/{}/{}", self.bucket, key);

        let mut file = File::open(local_path)
            .await
            .with_context(|| format!("Failed to open file: {}", local_path.display()))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).await?;

        let payload_hash = hex::encode(Sha256::digest(&bytes));
        let (amz_date, auth) = self.sign_request("PUT", &path, "", &payload_hash)?;

        let url = format!("{}{}", self.endpoint, path);
        let res = self
            .client
            .put(&url)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", &payload_hash)
            .header("Authorization", auth)
            .header("Content-Length", bytes.len())
            .body(bytes)
            .send()
            .await
            .with_context(|| format!("Failed S3 PUT for '{}'", remote_path))?;

        if !res.status().is_success() {
            let status = res.status();
            let err_text = res.text().await.unwrap_or_default();
            bail!(
                "S3 upload failed for '{}': HTTP {} - {}",
                remote_path,
                status,
                err_text
            );
        }

        Ok(())
    }

    async fn delete_file(&self, remote_path: &str) -> Result<()> {
        let key = self.key_for_path(remote_path);
        let path = format!("/{}/{}", self.bucket, key);
        let payload_hash = hex::encode(Sha256::digest(b""));
        let (amz_date, auth) = self.sign_request("DELETE", &path, "", &payload_hash)?;

        let url = format!("{}{}", self.endpoint, path);
        let res = self
            .client
            .delete(&url)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", &payload_hash)
            .header("Authorization", auth)
            .send()
            .await?;

        if !res.status().is_success() && res.status() != StatusCode::NOT_FOUND {
            let status = res.status();
            let err_text = res.text().await.unwrap_or_default();
            bail!(
                "S3 delete failed for '{}': HTTP {} - {}",
                remote_path,
                status,
                err_text
            );
        }

        Ok(())
    }

    async fn read_text_file(&self, remote_path: &str) -> Result<Vec<String>> {
        let key = self.key_for_path(remote_path);
        let path = format!("/{}/{}", self.bucket, key);
        let payload_hash = hex::encode(Sha256::digest(b""));
        let (amz_date, auth) = self.sign_request("GET", &path, "", &payload_hash)?;

        let url = format!("{}{}", self.endpoint, path);
        let res = self
            .client
            .get(&url)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", &payload_hash)
            .header("Authorization", auth)
            .send()
            .await?;

        if res.status() == StatusCode::NOT_FOUND {
            bail!("File '{}' not found in S3 bucket", remote_path);
        }

        if !res.status().is_success() {
            let status = res.status();
            let err_text = res.text().await.unwrap_or_default();
            bail!(
                "S3 read_text_file failed for '{}': HTTP {} - {}",
                remote_path,
                status,
                err_text
            );
        }

        let text = res.text().await.context("Failed to read text from S3")?;
        Ok(text
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect())
    }

    async fn list_dir(&self, remote_dir: &str) -> Result<Vec<RemoteFileInfo>> {
        let clean = remote_dir.trim().trim_matches('/');
        let prefix = if clean.is_empty() {
            "".to_string()
        } else {
            format!("{}/", clean)
        };

        let query = format!(
            "delimiter=/&list-type=2&prefix={}",
            urlencoding_encode(&prefix)
        );
        let path = format!("/{}", self.bucket);
        let payload_hash = hex::encode(Sha256::digest(b""));
        let (amz_date, auth) = self.sign_request("GET", &path, &query, &payload_hash)?;

        let url = format!("{}{}/?{}", self.endpoint, path, query);
        let res = self
            .client
            .get(&url)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", &payload_hash)
            .header("Authorization", auth)
            .send()
            .await?;

        if !res.status().is_success() {
            let status = res.status();
            let err_text = res.text().await.unwrap_or_default();
            bail!(
                "S3 list_dir failed for '{}': HTTP {} - {}",
                remote_dir,
                status,
                err_text
            );
        }

        let xml_text = res.text().await?;
        let parsed: ListBucketResult =
            from_str(&xml_text).context("Failed to parse S3 XML response")?;

        let dir_items = parsed.common_prefixes.into_iter().map(|p| {
            let folder_key = p.prefix.trim_matches('/').to_string();
            RemoteFileInfo {
                name: folder_key.rsplit('/').next().unwrap_or("").to_string(),
                path: format!("/{}", folder_key),
                is_dir: true,
                size: 0,
                md5: None,
                etag: None,
            }
        });

        let file_items = parsed.contents.into_iter().filter_map(|obj| {
            let obj_key = obj.key.trim_matches('/');
            if obj_key == clean || obj_key.ends_with('/') {
                return None;
            }

            let etag = obj.etag.map(|e| e.trim_matches('"').to_string());
            let md5 = etag
                .as_ref()
                .filter(|e| e.len() == 32 && !e.contains('-'))
                .cloned();

            Some(RemoteFileInfo {
                name: obj_key.rsplit('/').next().unwrap_or("").to_string(),
                path: format!("/{}", obj_key),
                is_dir: false,
                size: obj.size,
                md5,
                etag,
            })
        });

        Ok(dir_items.chain(file_items).collect())
    }
}

fn urlencoding_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
