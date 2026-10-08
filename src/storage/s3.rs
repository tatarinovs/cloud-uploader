//! S3-compatible object storage (AWS S3, Cloudflare R2, MinIO, Yandex Object
//! Storage, ...) using path-style requests signed with AWS Signature V4.

use super::{as_md5, parse_http_date, parse_rfc3339, RemoteFileInfo, StorageProvider};
use crate::client::{
    build_http_client, ensure_success, file_body, http_error, send, upload_timeout, API_TIMEOUT,
};
use crate::utils::format::format_bytes;
use crate::utils::path::{encode_component, encode_path, normalize_remote_dir};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, ETAG, LAST_MODIFIED};
use reqwest::{Client, Method, RequestBuilder, StatusCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::io::AsyncReadExt;
use tracing::{debug, warn};
use url::Url;

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
/// Files larger than this are uploaded in parts (bounded memory, resumable per part).
const MULTIPART_THRESHOLD: u64 = 64 * 1024 * 1024;
const MIN_PART_SIZE: u64 = 16 * 1024 * 1024;
const MAX_PARTS: u64 = 10_000;

struct Credentials {
    access_key: String,
    secret_key: String,
    region: String,
}

pub struct S3Provider {
    /// `scheme://host[:port]`
    origin: String,
    /// Value of the Host header (includes a non-default port).
    host: String,
    /// Already-encoded path prefix of the endpoint, usually empty.
    path_prefix: String,
    bucket: String,
    creds: Credentials,
    client: Client,
}

#[derive(Deserialize)]
struct ListBucketResult {
    #[serde(rename = "Contents", default)]
    contents: Vec<S3Object>,
    #[serde(rename = "CommonPrefixes", default)]
    common_prefixes: Vec<S3Prefix>,
    #[serde(rename = "IsTruncated", default)]
    is_truncated: bool,
    #[serde(rename = "NextContinuationToken")]
    next_continuation_token: Option<String>,
}

#[derive(Deserialize)]
struct S3Object {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "Size", default)]
    size: u64,
    #[serde(rename = "ETag")]
    etag: Option<String>,
    #[serde(rename = "LastModified")]
    last_modified: Option<String>,
}

#[derive(Deserialize)]
struct S3Prefix {
    #[serde(rename = "Prefix")]
    prefix: String,
}

#[derive(Deserialize)]
struct InitiateMultipartUploadResult {
    #[serde(rename = "UploadId")]
    upload_id: String,
}

impl S3Provider {
    pub fn new(
        endpoint: &str,
        bucket: String,
        access_key: String,
        secret_key: String,
        region: String,
    ) -> Result<Self> {
        let url = Url::parse(endpoint.trim())
            .with_context(|| format!("Invalid S3 endpoint '{endpoint}'"))?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("S3 endpoint must start with https:// or http://");
        }
        let host_name = url.host_str().context("S3 endpoint has no host")?;
        let host = match url.port() {
            Some(port) => format!("{host_name}:{port}"),
            None => host_name.to_string(),
        };
        Ok(Self {
            origin: format!("{}://{}", url.scheme(), host),
            host,
            path_prefix: url.path().trim_end_matches('/').to_string(),
            bucket,
            creds: Credentials {
                access_key,
                secret_key,
                region,
            },
            client: build_http_client()?,
        })
    }

    fn key_for(remote_path: &str) -> String {
        normalize_remote_dir(remote_path)
            .trim_start_matches('/')
            .to_string()
    }

    fn canonical_uri(&self, key: Option<&str>) -> String {
        let bucket = encode_component(&self.bucket);
        match key {
            Some(key) => format!("{}/{}/{}", self.path_prefix, bucket, encode_path(key)),
            None => format!("{}/{}", self.path_prefix, bucket),
        }
    }

    /// Builds a signed request. Call it once per attempt: the signature is time-bound.
    fn request(
        &self,
        method: Method,
        key: Option<&str>,
        query: &[(&str, &str)],
        payload_hash: &str,
    ) -> RequestBuilder {
        let uri = self.canonical_uri(key);
        let query = canonical_query(query);
        let amz_date = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let headers = [
            ("host", self.host.clone()),
            ("x-amz-content-sha256", payload_hash.to_string()),
            ("x-amz-date", amz_date.clone()),
        ];
        let auth = sign(
            &self.creds,
            method.as_str(),
            &uri,
            &query,
            &headers,
            payload_hash,
            &amz_date,
        );

        let url = if query.is_empty() {
            format!("{}{}", self.origin, uri)
        } else {
            format!("{}{}?{}", self.origin, uri, query)
        };
        self.client
            .request(method, url)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", payload_hash)
            .header(AUTHORIZATION, auth)
    }

    async fn list_objects(
        &self,
        prefix: &str,
        delimited: bool,
    ) -> Result<(Vec<S3Object>, Vec<S3Prefix>)> {
        let mut objects = Vec::new();
        let mut prefixes = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![("list-type", "2"), ("prefix", prefix)];
            if delimited {
                query.push(("delimiter", "/"));
            }
            if let Some(t) = token.as_deref() {
                query.push(("continuation-token", t));
            }
            let resp = send("s3: list objects", || {
                Ok(self
                    .request(Method::GET, None, &query, EMPTY_SHA256)
                    .timeout(API_TIMEOUT))
            })
            .await?;
            let xml = ensure_success(resp, format!("Failed to list '{prefix}'"))
                .await?
                .text()
                .await?;
            let page: ListBucketResult =
                quick_xml::de::from_str(&xml).context("Invalid S3 listing XML")?;
            objects.extend(page.contents);
            prefixes.extend(page.common_prefixes);
            match page.next_continuation_token {
                Some(next) if page.is_truncated => token = Some(next),
                _ => break,
            }
        }
        Ok((objects, prefixes))
    }

    async fn delete_object(&self, key: &str) -> Result<()> {
        let resp = send("s3: delete", || {
            Ok(self
                .request(Method::DELETE, Some(key), &[], EMPTY_SHA256)
                .timeout(API_TIMEOUT))
        })
        .await?;
        if resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(http_error(resp, format!("Failed to delete '{key}'")).await)
        }
    }

    async fn put_single(&self, local: &Path, key: &str) -> Result<()> {
        let resp = send(&format!("s3: upload '{key}'"), || {
            let (body, len) = file_body(local)?;
            Ok(self
                .request(Method::PUT, Some(key), &[], UNSIGNED_PAYLOAD)
                .header(CONTENT_LENGTH, len)
                .header(CONTENT_TYPE, "application/octet-stream")
                .timeout(upload_timeout(len))
                .body(body))
        })
        .await?;
        ensure_success(resp, format!("Upload of '{key}' failed")).await?;
        Ok(())
    }

    async fn put_multipart(&self, local: &Path, key: &str, size: u64) -> Result<()> {
        let resp = send("s3: create multipart upload", || {
            Ok(self
                .request(Method::POST, Some(key), &[("uploads", "")], EMPTY_SHA256)
                .timeout(API_TIMEOUT))
        })
        .await?;
        let xml = ensure_success(resp, format!("Failed to start multipart upload of '{key}'"))
            .await?
            .text()
            .await?;
        let init: InitiateMultipartUploadResult =
            quick_xml::de::from_str(&xml).context("Invalid CreateMultipartUpload response")?;

        let result = match self.upload_parts(local, key, &init.upload_id, size).await {
            Ok(etags) => self.complete_multipart(key, &init.upload_id, &etags).await,
            Err(err) => Err(err),
        };
        if result.is_err() {
            let abort = send("s3: abort multipart upload", || {
                Ok(self
                    .request(
                        Method::DELETE,
                        Some(key),
                        &[("uploadId", &init.upload_id)],
                        EMPTY_SHA256,
                    )
                    .timeout(API_TIMEOUT))
            })
            .await;
            if let Err(err) = abort {
                warn!("Failed to abort multipart upload of '{key}': {err:#}");
            }
        }
        result
    }

    async fn upload_parts(
        &self,
        local: &Path,
        key: &str,
        upload_id: &str,
        size: u64,
    ) -> Result<Vec<String>> {
        let part_size = MIN_PART_SIZE.max(size.div_ceil(MAX_PARTS));
        let total_parts = size.div_ceil(part_size);
        let mut file = tokio::fs::File::open(local)
            .await
            .with_context(|| format!("Failed to open '{}'", local.display()))?;

        let mut etags = Vec::with_capacity(total_parts as usize);
        let mut remaining = size;
        let mut part_number = 1u64;
        while remaining > 0 {
            let len = part_size.min(remaining);
            let mut buf = vec![0u8; len as usize];
            file.read_exact(&mut buf)
                .await
                .with_context(|| format!("Failed to read '{}'", local.display()))?;
            let data = Bytes::from(buf);
            let hash = hex::encode(Sha256::digest(&data));
            let number = part_number.to_string();

            let resp = send(
                &format!("s3: upload part {part_number}/{total_parts}"),
                || {
                    Ok(self
                        .request(
                            Method::PUT,
                            Some(key),
                            &[("partNumber", &number), ("uploadId", upload_id)],
                            &hash,
                        )
                        .header(CONTENT_LENGTH, data.len())
                        .timeout(upload_timeout(len))
                        .body(data.clone()))
                },
            )
            .await?;
            let resp = ensure_success(resp, format!("Upload of part {part_number} failed")).await?;
            let etag = resp
                .headers()
                .get(ETAG)
                .and_then(|v| v.to_str().ok())
                .context("S3 did not return an ETag for the uploaded part")?
                .to_string();
            etags.push(etag);

            remaining -= len;
            debug!(
                "'{key}': part {part_number}/{total_parts} done ({} left)",
                format_bytes(remaining)
            );
            part_number += 1;
        }
        Ok(etags)
    }

    async fn complete_multipart(&self, key: &str, upload_id: &str, etags: &[String]) -> Result<()> {
        let mut body = String::from("<CompleteMultipartUpload>");
        for (i, etag) in etags.iter().enumerate() {
            body.push_str(&format!(
                "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                i + 1,
                xml_escape(etag)
            ));
        }
        body.push_str("</CompleteMultipartUpload>");
        let body = Bytes::from(body);
        let hash = hex::encode(Sha256::digest(&body));

        let resp = send("s3: complete multipart upload", || {
            Ok(self
                .request(Method::POST, Some(key), &[("uploadId", upload_id)], &hash)
                .header(CONTENT_TYPE, "application/xml")
                .timeout(API_TIMEOUT * 10)
                .body(body.clone()))
        })
        .await?;
        let text = ensure_success(resp, format!("Failed to complete upload of '{key}'"))
            .await?
            .text()
            .await?;
        // S3 may report an error with HTTP 200 for this call.
        if text.contains("<Error>") {
            bail!("Failed to complete upload of '{key}': {text}");
        }
        Ok(())
    }
}

#[async_trait]
impl StorageProvider for S3Provider {
    fn name(&self) -> &'static str {
        "S3 compatible storage"
    }

    async fn check_access(&self, remote_dir: &str) -> Result<()> {
        let key = Self::key_for(remote_dir);
        let prefix = if key.is_empty() {
            key
        } else {
            format!("{key}/")
        };
        let resp = send("s3: check access", || {
            Ok(self
                .request(
                    Method::GET,
                    None,
                    &[("list-type", "2"), ("max-keys", "1"), ("prefix", &prefix)],
                    EMPTY_SHA256,
                )
                .timeout(API_TIMEOUT))
        })
        .await?;
        ensure_success(
            resp,
            format!(
                "Cannot access bucket '{}' (check S3_ENDPOINT, S3_BUCKET, S3_REGION and keys)",
                self.bucket
            ),
        )
        .await?;
        Ok(())
    }

    /// S3 has no real directories; prefixes appear implicitly with objects.
    async fn ensure_dir(&self, _remote_dir: &str) -> Result<()> {
        Ok(())
    }

    async fn get_file_info(&self, remote_path: &str) -> Result<Option<RemoteFileInfo>> {
        let key = Self::key_for(remote_path);
        if key.is_empty() {
            return Ok(None);
        }
        let resp = send("s3: HEAD", || {
            Ok(self
                .request(Method::HEAD, Some(&key), &[], EMPTY_SHA256)
                .timeout(API_TIMEOUT))
        })
        .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = ensure_success(resp, format!("Failed to get info for '{key}'")).await?;
        let header = |name| resp.headers().get(name).and_then(|v| v.to_str().ok());

        Ok(Some(RemoteFileInfo {
            name: key.rsplit('/').next().unwrap_or_default().to_string(),
            path: format!("/{key}"),
            is_dir: false,
            size: header(CONTENT_LENGTH)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            // Multipart ETags ("...-N") are not MD5 hashes; as_md5 rejects them.
            md5: header(ETAG).and_then(as_md5),
            modified: header(LAST_MODIFIED).and_then(parse_http_date),
        }))
    }

    async fn upload_file(&self, local_path: &Path, remote_path: &str) -> Result<()> {
        let key = Self::key_for(remote_path);
        if key.is_empty() {
            bail!("Invalid S3 object key for '{remote_path}'");
        }
        let size = tokio::fs::metadata(local_path)
            .await
            .with_context(|| format!("Failed to read metadata of '{}'", local_path.display()))?
            .len();
        if size > MULTIPART_THRESHOLD {
            self.put_multipart(local_path, &key, size).await
        } else {
            self.put_single(local_path, &key).await
        }
    }

    async fn delete(&self, remote_path: &str, is_dir: bool) -> Result<()> {
        let key = Self::key_for(remote_path);
        if key.is_empty() {
            bail!("Refusing to delete the whole bucket");
        }
        if !is_dir {
            return self.delete_object(&key).await;
        }
        let prefix = format!("{key}/");
        let (objects, _) = self.list_objects(&prefix, false).await?;
        for object in &objects {
            self.delete_object(&object.key).await?;
        }
        // Folder marker object created by other tools, if any.
        self.delete_object(&prefix).await
    }

    async fn read_text_file(&self, remote_path: &str) -> Result<Option<String>> {
        let key = Self::key_for(remote_path);
        let resp = send("s3: GET", || {
            Ok(self
                .request(Method::GET, Some(&key), &[], EMPTY_SHA256)
                .timeout(API_TIMEOUT))
        })
        .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let text = ensure_success(resp, format!("Failed to read '{key}'"))
            .await?
            .text()
            .await?;
        Ok(Some(text))
    }

    async fn list_dir(&self, remote_dir: &str) -> Result<Vec<RemoteFileInfo>> {
        let key = Self::key_for(remote_dir);
        let prefix = if key.is_empty() {
            key
        } else {
            format!("{key}/")
        };
        let (objects, prefixes) = self.list_objects(&prefix, true).await?;

        let dirs = prefixes.into_iter().map(|p| {
            let path = p.prefix.trim_end_matches('/').to_string();
            RemoteFileInfo {
                name: path.rsplit('/').next().unwrap_or_default().to_string(),
                path: format!("/{path}"),
                is_dir: true,
                size: 0,
                md5: None,
                modified: None,
            }
        });
        let files = objects
            .into_iter()
            .filter(|o| o.key != prefix && !o.key.ends_with('/'))
            .map(|o| RemoteFileInfo {
                name: o.key.rsplit('/').next().unwrap_or_default().to_string(),
                path: format!("/{}", o.key),
                is_dir: false,
                size: o.size,
                md5: o.etag.as_deref().and_then(as_md5),
                modified: o.last_modified.as_deref().and_then(parse_rfc3339),
            });
        Ok(dirs.chain(files).collect())
    }
}

/// SigV4 canonical query string: every key and value URI-encoded, sorted.
fn canonical_query(params: &[(&str, &str)]) -> String {
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (encode_component(k), encode_component(v)))
        .collect();
    encoded.sort();
    encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Computes the AWS Signature V4 `Authorization` header value.
/// `uri` and `query` must already be in canonical (encoded) form.
fn sign(
    creds: &Credentials,
    method: &str,
    uri: &str,
    query: &str,
    headers: &[(&str, String)],
    payload_hash: &str,
    amz_date: &str,
) -> String {
    let mut headers: Vec<(String, &str)> = headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim()))
        .collect();
    headers.sort();
    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect();
    let signed_headers = headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request =
        format!("{method}\n{uri}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
    tracing::trace!("SigV4 canonical request:\n{canonical_request}");
    let date = &amz_date[..8];
    let scope = format!("{date}/{}/s3/aws4_request", creds.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );

    let k_date = hmac(
        format!("AWS4{}", creds.secret_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac(&k_date, creds.region.as_bytes());
    let k_service = hmac(&k_region, b"s3");
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex::encode(hmac(&k_signing, string_to_sign.as_bytes()));

    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key
    )
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_creds() -> Credentials {
        Credentials {
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            region: "us-east-1".into(),
        }
    }

    /// "GET Object" example from the AWS SigV4 documentation.
    #[test]
    fn sigv4_matches_aws_get_object_example() {
        let headers = [
            ("Host", "examplebucket.s3.amazonaws.com".to_string()),
            ("Range", "bytes=0-9".to_string()),
            ("x-amz-content-sha256", EMPTY_SHA256.to_string()),
            ("x-amz-date", "20130524T000000Z".to_string()),
        ];
        let auth = sign(
            &example_creds(),
            "GET",
            "/test.txt",
            "",
            &headers,
            EMPTY_SHA256,
            "20130524T000000Z",
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    /// "GET Bucket (List Objects)" example from the AWS SigV4 documentation.
    #[test]
    fn sigv4_matches_aws_list_objects_example() {
        let headers = [
            ("host", "examplebucket.s3.amazonaws.com".to_string()),
            ("x-amz-content-sha256", EMPTY_SHA256.to_string()),
            ("x-amz-date", "20130524T000000Z".to_string()),
        ];
        let query = canonical_query(&[("prefix", "J"), ("max-keys", "2")]);
        assert_eq!(query, "max-keys=2&prefix=J");
        let auth = sign(
            &example_creds(),
            "GET",
            "/",
            &query,
            &headers,
            EMPTY_SHA256,
            "20130524T000000Z",
        );
        assert!(auth.ends_with(
            "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        ));
    }

    #[test]
    fn canonical_uri_and_query_are_encoded() {
        let p = S3Provider::new(
            "http://localhost:9000/",
            "backups".into(),
            "a".into(),
            "b".into(),
            "us-east-1".into(),
        )
        .unwrap();
        assert_eq!(p.host, "localhost:9000");
        assert_eq!(p.origin, "http://localhost:9000");
        assert_eq!(
            p.canonical_uri(Some("repo/[v1.0] my app.apk")),
            "/backups/repo/%5Bv1.0%5D%20my%20app.apk"
        );
        assert_eq!(
            canonical_query(&[("prefix", "a b/"), ("delimiter", "/"), ("list-type", "2")]),
            "delimiter=%2F&list-type=2&prefix=a%20b%2F"
        );

        let r2 = S3Provider::new(
            "https://acc.r2.cloudflarestorage.com",
            "b".into(),
            "a".into(),
            "b".into(),
            "auto".into(),
        )
        .unwrap();
        assert_eq!(r2.host, "acc.r2.cloudflarestorage.com");
    }

    #[test]
    fn parses_list_result() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>b</Name><Prefix>Upload/</Prefix><KeyCount>2</KeyCount>
  <IsTruncated>true</IsTruncated>
  <NextContinuationToken>abc</NextContinuationToken>
  <Contents><Key>Upload/a &amp; b.zip</Key><Size>10</Size><ETag>"d41d8cd98f00b204e9800998ecf8427e"</ETag><LastModified>2024-01-01T00:00:00.000Z</LastModified></Contents>
  <Contents><Key>Upload/big.bin</Key><Size>99</Size><ETag>"abc-3"</ETag></Contents>
  <CommonPrefixes><Prefix>Upload/sub/</Prefix></CommonPrefixes>
</ListBucketResult>"#;
        let r: ListBucketResult = quick_xml::de::from_str(xml).unwrap();
        assert!(r.is_truncated);
        assert_eq!(r.next_continuation_token.as_deref(), Some("abc"));
        assert_eq!(r.contents.len(), 2);
        assert_eq!(r.contents[0].key, "Upload/a & b.zip");
        assert_eq!(r.common_prefixes[0].prefix, "Upload/sub/");
    }
}
