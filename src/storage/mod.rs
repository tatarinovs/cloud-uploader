pub mod google;
pub mod s3;
pub mod webdav;
pub mod yandex;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteFileInfo {
    pub name: String,
    /// Normalized absolute remote path, e.g. `/Upload/file.zip`.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Lowercase hex MD5, set only when the provider guarantees it is a content hash.
    pub md5: Option<String>,
    pub modified: Option<DateTime<Utc>>,
}

/// A cloud storage backend. All paths are absolute, `/`-separated and
/// normalized (see [`crate::utils::path::normalize_remote_dir`]).
#[async_trait]
pub trait StorageProvider: Send + Sync {
    fn name(&self) -> &'static str;

    /// Verifies credentials and makes sure `remote_dir` is usable (may create it).
    async fn check_access(&self, remote_dir: &str) -> Result<()> {
        self.ensure_dir(remote_dir).await
    }

    /// Creates `remote_dir` with all missing parents. Succeeds if it already exists.
    async fn ensure_dir(&self, remote_dir: &str) -> Result<()>;

    /// Returns `None` if nothing exists at `remote_path`.
    async fn get_file_info(&self, remote_path: &str) -> Result<Option<RemoteFileInfo>>;

    /// Uploads or overwrites a file. The parent directory must already exist.
    async fn upload_file(&self, local_path: &Path, remote_path: &str) -> Result<()>;

    /// Deletes a file, or a directory with all of its content.
    /// A path that does not exist is not an error.
    async fn delete(&self, remote_path: &str, is_dir: bool) -> Result<()>;

    /// Returns `None` if the file does not exist.
    async fn read_text_file(&self, remote_path: &str) -> Result<Option<String>>;

    /// Lists the direct children of `remote_dir`; empty if it does not exist.
    async fn list_dir(&self, remote_dir: &str) -> Result<Vec<RemoteFileInfo>>;
}

pub(crate) fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Parses an HTTP-date such as `Tue, 15 Nov 1994 12:45:26 GMT`.
pub(crate) fn parse_http_date(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc2822(s.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Returns the value lowercased if it looks like an MD5 hex digest.
pub(crate) fn as_md5(s: &str) -> Option<String> {
    let s = s.trim().trim_matches('"');
    (s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit())).then(|| s.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dates_and_md5() {
        assert!(parse_http_date("Tue, 15 Nov 1994 12:45:26 GMT").is_some());
        assert!(parse_rfc3339("2009-10-12T17:50:30.000Z").is_some());
        assert_eq!(
            as_md5("\"D41D8CD98F00B204E9800998ECF8427E\"").as_deref(),
            Some("d41d8cd98f00b204e9800998ecf8427e")
        );
        assert_eq!(as_md5("abc-2"), None);
    }
}
