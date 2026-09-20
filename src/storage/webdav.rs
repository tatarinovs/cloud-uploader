use crate::client::{build_http_client, retry_request};
use crate::storage::{RemoteFileInfo, StorageProvider};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use quick_xml::de::from_str;
use reqwest::{Client, Method, StatusCode};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;
use tokio::fs::File;
use url::Url;

pub struct WebDavProvider {
    display_name: &'static str,
    base_url: String,
    user: String,
    password: String,
    trust_etag_as_md5: bool,
    client: Client,
}

impl WebDavProvider {
    pub fn new(
        display_name: &'static str,
        base_url: String,
        user: String,
        password: String,
    ) -> Result<Self> {
        Self::new_with_etag_option(display_name, base_url, user, password, true)
    }

    pub fn new_with_etag_option(
        display_name: &'static str,
        base_url: String,
        user: String,
        password: String,
        trust_etag_as_md5: bool,
    ) -> Result<Self> {
        Ok(Self {
            display_name,
            base_url: base_url.trim_end_matches('/').to_string(),
            user,
            password,
            trust_etag_as_md5,
            client: build_http_client(Some(Duration::from_secs(600)))?,
        })
    }

    fn build_url(&self, remote_path: &str) -> String {
        let clean = remote_path.trim_matches('/');
        if clean.is_empty() {
            format!("{}/", self.base_url)
        } else {
            let encoded_segments: Vec<String> = clean
                .split('/')
                .map(|seg| url::form_urlencoded::byte_serialize(seg.as_bytes()).collect::<String>())
                .collect();
            format!("{}/{}", self.base_url, encoded_segments.join("/"))
        }
    }
}

#[derive(Debug, Deserialize)]
struct DavMultistatus {
    #[serde(rename = "response", default)]
    responses: Vec<DavResponse>,
}

#[derive(Debug, Deserialize)]
struct DavResponse {
    href: String,
    propstat: Option<DavPropstat>,
}

#[derive(Debug, Deserialize)]
struct DavPropstat {
    prop: Option<DavProp>,
}

#[derive(Debug, Deserialize)]
struct DavProp {
    #[serde(default)]
    getcontentlength: Option<String>,
    #[serde(default)]
    getetag: Option<String>,
    #[serde(default)]
    getlastmodified: Option<String>,
    resourcetype: Option<DavResourceType>,
}

#[derive(Debug, Deserialize)]
struct DavResourceType {
    collection: Option<DavEmpty>,
}

#[derive(Debug, Deserialize)]
struct DavEmpty {}

#[async_trait]
impl StorageProvider for WebDavProvider {
    fn name(&self) -> &'static str {
        self.display_name
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

            let url = self.build_url(&current);
            let mkcol_method = Method::from_bytes(b"MKCOL").unwrap();

            let res = retry_request("webdav_mkcol", || {
                self.client
                    .request(mkcol_method.clone(), &url)
                    .basic_auth(&self.user, Some(&self.password))
                    .send()
            })
            .await
            .with_context(|| format!("Failed MKCOL for directory '{}'", current))?;

            let status = res.status();
            if status != StatusCode::CREATED
                && status != StatusCode::OK
                && status != StatusCode::METHOD_NOT_ALLOWED
                && status != StatusCode::CONFLICT
            {
                let err_text = res.text().await.unwrap_or_default();
                bail!(
                    "Failed to create WebDAV directory '{}': HTTP {} - {}",
                    current,
                    status,
                    err_text
                );
            }
        }

        Ok(())
    }

    async fn get_file_info(&self, remote_path: &str) -> Result<Option<RemoteFileInfo>> {
        let url = self.build_url(remote_path);
        let propfind_method = Method::from_bytes(b"PROPFIND").unwrap();
        let prop_body = r#"<?xml version="1.0" encoding="utf-8" ?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:getcontentlength/>
    <D:getetag/>
    <D:getlastmodified/>
    <D:resourcetype/>
  </D:prop>
</D:propfind>"#;

        let res = match self
            .client
            .request(propfind_method, &url)
            .basic_auth(&self.user, Some(&self.password))
            .header("Depth", "0")
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(prop_body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };

        let status = res.status();
        // 404 (Not Found), 400 (Bad Request), or 500 (Mail.ru quirk on non-existent paths)
        if status == StatusCode::NOT_FOUND
            || status == StatusCode::BAD_REQUEST
            || status == StatusCode::INTERNAL_SERVER_ERROR
        {
            return Ok(None);
        }

        if !status.is_success() && status != StatusCode::MULTI_STATUS {
            return Ok(None);
        }

        let text = res.text().await.unwrap_or_default();
        let multi: DavMultistatus = match from_str(&text) {
            Ok(m) => m,
            Err(_) => return Ok(None),
        };

        let response = match multi.responses.first() {
            Some(r) => r,
            None => return Ok(None),
        };

        let prop = match response.propstat.as_ref().and_then(|ps| ps.prop.as_ref()) {
            Some(p) => p,
            None => return Ok(None),
        };

        let is_dir = prop
            .resourcetype
            .as_ref()
            .map(|rt| rt.collection.is_some())
            .unwrap_or(false);
        let size: i64 = prop
            .getcontentlength
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let etag = prop
            .getetag
            .as_deref()
            .map(|s| s.trim_matches('"').to_string());
        let md5 = if self.trust_etag_as_md5 {
            parse_etag_md5(etag.as_deref())
        } else {
            None
        };
        let last_modified = prop
            .getlastmodified
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc2822(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc));
        let name = remote_path.rsplit('/').next().unwrap_or("").to_string();

        Ok(Some(RemoteFileInfo {
            name,
            path: remote_path.to_string(),
            is_dir,
            size,
            md5,
            etag,
            last_modified,
        }))
    }

    async fn upload_file(&self, local_path: &Path, remote_path: &str) -> Result<()> {
        let file = File::open(local_path)
            .await
            .with_context(|| format!("Failed to open file for upload: {}", local_path.display()))?;
        let metadata = file.metadata().await?;
        let file_len = metadata.len();

        let url = self.build_url(remote_path);
        let res = self
            .client
            .put(&url)
            .basic_auth(&self.user, Some(&self.password))
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", file_len)
            .body(file)
            .send()
            .await
            .with_context(|| format!("Failed WebDAV PUT to '{}'", remote_path))?;

        let status = res.status();
        if status != StatusCode::CREATED
            && status != StatusCode::OK
            && status != StatusCode::NO_CONTENT
        {
            let err_text = res.text().await.unwrap_or_default();
            bail!(
                "WebDAV PUT failed for '{}': HTTP {} - {}",
                remote_path,
                status,
                err_text
            );
        }

        Ok(())
    }

    async fn delete_file(&self, remote_path: &str) -> Result<()> {
        let url = self.build_url(remote_path);

        let res = retry_request("webdav_delete", || {
            self.client
                .delete(&url)
                .basic_auth(&self.user, Some(&self.password))
                .send()
        })
        .await
        .with_context(|| format!("Failed WebDAV DELETE for '{}'", remote_path))?;

        let status = res.status();
        if status != StatusCode::OK
            && status != StatusCode::NO_CONTENT
            && status != StatusCode::NOT_FOUND
        {
            let err_text = res.text().await.unwrap_or_default();
            bail!(
                "WebDAV DELETE failed for '{}': HTTP {} - {}",
                remote_path,
                status,
                err_text
            );
        }

        Ok(())
    }

    async fn read_text_file(&self, remote_path: &str) -> Result<Vec<String>> {
        let url = self.build_url(remote_path);

        let res = retry_request("webdav_get_text", || {
            self.client
                .get(&url)
                .basic_auth(&self.user, Some(&self.password))
                .send()
        })
        .await
        .with_context(|| format!("Failed to read '{}'", remote_path))?;

        if res.status() == StatusCode::NOT_FOUND {
            bail!("File '{}' not found in WebDAV storage", remote_path);
        }

        if !res.status().is_success() {
            let status = res.status();
            let err_text = res.text().await.unwrap_or_default();
            bail!(
                "WebDAV GET failed for '{}': HTTP {} - {}",
                remote_path,
                status,
                err_text
            );
        }

        let text = res.text().await.context("Failed to read text body")?;
        Ok(text
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect())
    }

    async fn list_dir(&self, remote_dir: &str) -> Result<Vec<RemoteFileInfo>> {
        let url = self.build_url(remote_dir);
        let propfind_method = Method::from_bytes(b"PROPFIND").unwrap();
        let propfind_body = r#"<?xml version="1.0" encoding="utf-8" ?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:getcontentlength/>
    <D:resourcetype/>
    <D:getetag/>
    <D:getlastmodified/>
  </D:prop>
</D:propfind>"#;

        let res = retry_request("webdav_propfind", || {
            self.client
                .request(propfind_method.clone(), &url)
                .basic_auth(&self.user, Some(&self.password))
                .header("Depth", "1")
                .header("Content-Type", "application/xml; charset=utf-8")
                .body(propfind_body)
                .send()
        })
        .await
        .with_context(|| format!("Failed PROPFIND for directory '{}'", remote_dir))?;

        if res.status() == StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }

        if !res.status().is_success() && res.status() != StatusCode::MULTI_STATUS {
            let status = res.status();
            let err_text = res.text().await.unwrap_or_default();
            bail!(
                "WebDAV PROPFIND failed for '{}': HTTP {} - {}",
                remote_dir,
                status,
                err_text
            );
        }

        let text = res
            .text()
            .await
            .context("Failed to read PROPFIND response")?;
        let multi: DavMultistatus =
            from_str(&text).context("Failed to parse WebDAV XML response")?;

        let clean_dir = remote_dir.trim().trim_matches('/');

        let items = multi
            .responses
            .into_iter()
            .filter_map(|r| {
                let decoded_href = match Url::parse(&r.href) {
                    Ok(u) => u.path().to_string(),
                    Err(_) => urlencoding_decode(&r.href).unwrap_or(r.href),
                };

                let item_path = decoded_href.trim_matches('/');
                if item_path == clean_dir || item_path.trim_end_matches('/') == clean_dir {
                    return None;
                }

                let name = item_path.rsplit('/').next()?.to_string();
                if name.is_empty() {
                    return None;
                }

                let prop = r.propstat.as_ref().and_then(|ps| ps.prop.as_ref());
                let is_dir = prop
                    .and_then(|p| p.resourcetype.as_ref())
                    .map(|rt| rt.collection.is_some())
                    .unwrap_or(false);
                let size: i64 = prop
                    .and_then(|p| p.getcontentlength.as_deref())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let etag = prop
                    .and_then(|p| p.getetag.as_deref())
                    .map(|s| s.trim_matches('"').to_string());
                let md5 = if self.trust_etag_as_md5 {
                    parse_etag_md5(etag.as_deref())
                } else {
                    None
                };
                let last_modified = prop
                    .and_then(|p| p.getlastmodified.as_deref())
                    .and_then(|s| chrono::DateTime::parse_from_rfc2822(s).ok())
                    .map(|dt| dt.with_timezone(&chrono::Utc));

                Some(RemoteFileInfo {
                    name,
                    path: format!("/{}", item_path),
                    is_dir,
                    size,
                    md5,
                    etag,
                    last_modified,
                })
            })
            .collect();

        Ok(items)
    }
}

fn parse_etag_md5(etag: Option<&str>) -> Option<String> {
    etag.filter(|s| s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|s| s.to_lowercase())
}

fn urlencoding_decode(s: &str) -> Option<String> {
    url::form_urlencoded::parse(s.as_bytes())
        .map(|(k, _)| k.to_string())
        .next()
}
