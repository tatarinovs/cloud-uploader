pub mod google;
pub mod mailru;
pub mod s3;
pub mod webdav;
pub mod yandex;

use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct RemoteFileInfo {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: i64,
    pub md5: Option<String>,
    #[allow(dead_code)]
    pub etag: Option<String>,
    pub last_modified: Option<chrono::DateTime<chrono::Utc>>,
}

#[async_trait]
pub trait StorageProvider: Send + Sync {
    fn name(&self) -> &'static str;

    async fn ensure_dir(&self, remote_dir: &str) -> Result<()>;

    async fn upload_file(&self, local_path: &Path, remote_path: &str) -> Result<()>;

    async fn get_file_info(&self, remote_path: &str) -> Result<Option<RemoteFileInfo>>;

    async fn delete_file(&self, remote_path: &str) -> Result<()>;

    async fn read_text_file(&self, remote_path: &str) -> Result<Vec<String>>;

    async fn list_dir(&self, remote_dir: &str) -> Result<Vec<RemoteFileInfo>>;
}
