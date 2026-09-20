use crate::storage::webdav::WebDavProvider;
use anyhow::Result;

pub struct MailRuProvider {
    inner: WebDavProvider,
}

impl MailRuProvider {
    pub fn new(user: String, password: String) -> Result<Self> {
        Ok(Self {
            inner: WebDavProvider::new_with_etag_option(
                "Mail.ru Cloud (WebDAV)",
                "https://webdav.mail.ru".to_string(),
                user,
                password,
                false, // Mail.ru WebDAV ETag is an internal revision tag, not MD5
            )?,
        })
    }
}

// Delegate StorageProvider implementation to inner WebDavProvider
#[async_trait::async_trait]
impl crate::storage::StorageProvider for MailRuProvider {
    fn name(&self) -> &'static str {
        "Mail.ru Cloud (WebDAV)"
    }

    async fn ensure_dir(&self, remote_dir: &str) -> Result<()> {
        self.inner.ensure_dir(remote_dir).await
    }

    async fn upload_file(&self, local_path: &std::path::Path, remote_path: &str) -> Result<()> {
        self.inner.upload_file(local_path, remote_path).await
    }

    async fn get_file_info(
        &self,
        remote_path: &str,
    ) -> Result<Option<crate::storage::RemoteFileInfo>> {
        self.inner.get_file_info(remote_path).await
    }

    async fn delete_file(&self, remote_path: &str) -> Result<()> {
        self.inner.delete_file(remote_path).await
    }

    async fn read_text_file(&self, remote_path: &str) -> Result<Vec<String>> {
        self.inner.read_text_file(remote_path).await
    }

    async fn list_dir(&self, remote_dir: &str) -> Result<Vec<crate::storage::RemoteFileInfo>> {
        self.inner.list_dir(remote_dir).await
    }
}
