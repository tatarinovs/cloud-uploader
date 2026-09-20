use crate::storage::StorageProvider;
use crate::utils::format::format_bytes;
use crate::utils::hash::compute_md5_file;
use crate::utils::path::normalize_remote_dir;
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::path::Path;
use tracing::{info, warn};
use walkdir::WalkDir;

pub async fn run_local_upload(
    provider: &dyn StorageProvider,
    local_path: &Path,
    remote_base_dir: &str,
    force_overwrite: bool,
) -> Result<()> {
    if !local_path.exists() {
        bail!("Local path does not exist: {}", local_path.display());
    }

    let metadata = local_path
        .metadata()
        .with_context(|| format!("Failed to read metadata for {}", local_path.display()))?;

    let clean_base = normalize_remote_dir(remote_base_dir);

    if metadata.is_file() {
        // Single file upload
        let file_name = local_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("uploaded_file");
        let remote_file_path = if clean_base == "/" {
            format!("/{}", file_name)
        } else {
            format!("{}/{}", clean_base.trim_end_matches('/'), file_name)
        };

        info!(
            "Uploading single file '{}' -> '{}' ({})",
            local_path.display(),
            remote_file_path,
            format_bytes(metadata.len() as i64)
        );

        let local_mtime = metadata.modified().ok();
        if should_skip_upload(
            provider,
            local_path,
            &remote_file_path,
            metadata.len() as i64,
            local_mtime,
            force_overwrite,
        )
        .await
        {
            info!(
                "  [*] Remote file '{}' already exists with identical size/hash. Skipping.",
                remote_file_path
            );
            return Ok(());
        }

        provider
            .upload_file(local_path, &remote_file_path)
            .await
            .with_context(|| format!("Failed to upload file to {}", remote_file_path))?;
        info!("  [+] Successfully uploaded: {}", remote_file_path);
        return Ok(());
    }

    // Directory recursive upload
    info!(
        "Recursively uploading directory '{}' -> '{}'...",
        local_path.display(),
        clean_base
    );

    let mut created_dirs: HashSet<String> = HashSet::new();
    created_dirs.insert(clean_base.clone());

    let mut total_files: u64 = 0;
    let mut total_bytes: i64 = 0;
    let mut uploaded_files: u64 = 0;
    let mut skipped_files: u64 = 0;

    for entry_result in WalkDir::new(local_path).follow_links(false) {
        let entry = match entry_result {
            Ok(e) => e,
            Err(e) => {
                warn!("  [!] Error accessing path during traversal: {}", e);
                continue;
            }
        };

        let current_path = entry.path();
        let rel_path = match current_path.strip_prefix(local_path) {
            Ok(r) => r,
            Err(_) => continue,
        };

        if rel_path.as_os_str().is_empty() {
            continue;
        }

        let rel_str = rel_path.to_string_lossy().replace('\\', "/");
        let remote_target = if clean_base == "/" {
            format!("/{}", rel_str)
        } else {
            format!("{}/{}", clean_base.trim_end_matches('/'), rel_str)
        };

        if entry.file_type().is_dir() {
            if !created_dirs.contains(&remote_target) {
                if let Err(e) = provider.ensure_dir(&remote_target).await {
                    warn!(
                        "  [!] Failed to create remote directory '{}': {}",
                        remote_target, e
                    );
                } else {
                    created_dirs.insert(remote_target);
                }
            }
            continue;
        }

        // It is a file
        let file_meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                warn!(
                    "  [!] Failed to get metadata for '{}': {}",
                    current_path.display(),
                    e
                );
                continue;
            }
        };

        let file_size = file_meta.len() as i64;
        let file_mtime = file_meta.modified().ok();
        total_files += 1;
        total_bytes += file_size;

        // Ensure parent folder exists
        let parent_remote = if let Some(idx) = remote_target.rfind('/') {
            let p = &remote_target[..idx];
            if p.is_empty() {
                "/".to_string()
            } else {
                p.to_string()
            }
        } else {
            "/".to_string()
        };

        if !created_dirs.contains(&parent_remote) {
            if let Err(e) = provider.ensure_dir(&parent_remote).await {
                warn!(
                    "  [!] Failed to ensure parent directory '{}': {}",
                    parent_remote, e
                );
            } else {
                created_dirs.insert(parent_remote);
            }
        }

        if should_skip_upload(
            provider,
            current_path,
            &remote_target,
            file_size,
            file_mtime,
            force_overwrite,
        )
        .await
        {
            info!("  [*] Skip (already exists): {}", remote_target);
            skipped_files += 1;
            continue;
        }

        info!(
            "  [^] Uploading: {} ({})...",
            rel_str,
            format_bytes(file_size)
        );
        match provider.upload_file(current_path, &remote_target).await {
            Ok(_) => {
                uploaded_files += 1;
                info!("  [+] Uploaded: {}", remote_target);
            }
            Err(e) => {
                warn!("  [!] Upload failed for '{}': {}", remote_target, e);
            }
        }
    }

    info!("==================================================");
    info!("Backup upload finished!");
    info!(
        "Total files scanned: {} ({})",
        total_files,
        format_bytes(total_bytes)
    );
    info!("Files uploaded:      {}", uploaded_files);
    info!("Files skipped:       {}", skipped_files);
    info!("==================================================");

    Ok(())
}

pub async fn should_skip_upload(
    provider: &dyn StorageProvider,
    local_path: &Path,
    remote_path: &str,
    local_size: i64,
    local_modified: Option<std::time::SystemTime>,
    force_overwrite: bool,
) -> bool {
    if force_overwrite {
        return false;
    }

    let remote_info = match provider.get_file_info(remote_path).await {
        Ok(Some(info)) => info,
        _ => return false,
    };

    // 1. Compare sizes: if sizes differ, definitely not the same file
    if remote_info.size != local_size {
        return false;
    }

    // 2. If remote has an MD5 checksum, compare MD5 hashes strictly
    if let Some(ref remote_md5) = remote_info.md5 {
        if !remote_md5.is_empty() {
            match compute_md5_file(local_path).await {
                Ok(local_md5) => {
                    let matches = local_md5.eq_ignore_ascii_case(remote_md5);
                    if !matches {
                        info!(
                            "  [~] Size matches but MD5 differs for '{}'. Re-uploading...",
                            remote_path
                        );
                    }
                    return matches;
                }
                Err(e) => {
                    warn!(
                        "  [!] Failed to compute local MD5 for '{}': {}",
                        local_path.display(),
                        e
                    );
                    return false;
                }
            }
        }
    }

    // 3. If no MD5 is available (e.g. Mail.ru Cloud / WebDAV), check modification timestamp:
    // This is critical for multi-volume archives or split backups where all volumes
    // have the exact same split size (e.g. 100MB), but new content has been generated.
    if let (Some(loc_mtime), Some(rem_mtime)) = (local_modified, remote_info.last_modified) {
        let loc_dt: chrono::DateTime<chrono::Utc> = loc_mtime.into();
        // Allow a small 2-second tolerance for file system timestamp rounding differences
        if loc_dt > rem_mtime + chrono::Duration::seconds(2) {
            info!(
                "  [~] Size matches but local file is newer for '{}' (local: {}, remote: {}). Re-uploading...",
                remote_path,
                loc_dt.format("%Y-%m-%d %H:%M:%S UTC"),
                rem_mtime.format("%Y-%m-%d %H:%M:%S UTC")
            );
            return false;
        }
    }

    // 4. Remote file exists and has the EXACT same size (remote_info.size == local_size).
    // If the storage protocol does not provide an MD5 hash, but modification timestamps
    // indicate the file is not newer, treat it as already uploaded.
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::RemoteFileInfo;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct FakeProvider {
        files: Mutex<HashMap<String, RemoteFileInfo>>,
    }

    #[async_trait]
    impl StorageProvider for FakeProvider {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn ensure_dir(&self, _remote_dir: &str) -> Result<()> {
            Ok(())
        }
        async fn upload_file(&self, _local_path: &Path, _remote_path: &str) -> Result<()> {
            Ok(())
        }
        async fn get_file_info(&self, remote_path: &str) -> Result<Option<RemoteFileInfo>> {
            Ok(self.files.lock().unwrap().get(remote_path).cloned())
        }
        async fn delete_file(&self, remote_path: &str) -> Result<()> {
            self.files.lock().unwrap().remove(remote_path);
            Ok(())
        }
        async fn read_text_file(&self, _remote_path: &str) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        async fn list_dir(&self, _remote_dir: &str) -> Result<Vec<RemoteFileInfo>> {
            Ok(self.files.lock().unwrap().values().cloned().collect())
        }
    }

    #[tokio::test]
    async fn test_should_skip_upload_reuploads_on_hash_mismatch_same_size() {
        let dir = tempfile::tempdir().unwrap();
        let local_file = dir.path().join("backup.part01.rar");
        tokio::fs::write(&local_file, b"NEW_DATA_CONTENT").await.unwrap();
        let local_size = 16i64;

        let mut files = HashMap::new();
        files.insert(
            "/Backups/backup.part01.rar".to_string(),
            RemoteFileInfo {
                name: "backup.part01.rar".to_string(),
                path: "/Backups/backup.part01.rar".to_string(),
                is_dir: false,
                size: local_size,
                md5: Some("0123456789abcdef0123456789abcdef".to_string()),
                etag: None,
                last_modified: None,
            },
        );

        let provider = FakeProvider {
            files: Mutex::new(files),
        };

        let skip = should_skip_upload(
            &provider,
            &local_file,
            "/Backups/backup.part01.rar",
            local_size,
            None,
            false,
        )
        .await;

        assert!(!skip, "Should re-upload when MD5 differs even if size matches");
    }

    #[tokio::test]
    async fn test_should_skip_upload_multivolume_archive_when_local_newer_no_hash() {
        let dir = tempfile::tempdir().unwrap();
        let local_file = dir.path().join("backup.part01.rar");
        tokio::fs::write(&local_file, b"SAME_SIZE_BYTES!").await.unwrap();
        let local_size = 16i64;

        let now = std::time::SystemTime::now();
        let remote_time = chrono::Utc::now() - chrono::Duration::hours(24);

        let mut files = HashMap::new();
        files.insert(
            "/Backups/backup.part01.rar".to_string(),
            RemoteFileInfo {
                name: "backup.part01.rar".to_string(),
                path: "/Backups/backup.part01.rar".to_string(),
                is_dir: false,
                size: local_size,
                md5: None, // No MD5 (Mail.ru case)
                etag: None,
                last_modified: Some(remote_time),
            },
        );

        let provider = FakeProvider {
            files: Mutex::new(files),
        };

        let skip = should_skip_upload(
            &provider,
            &local_file,
            "/Backups/backup.part01.rar",
            local_size,
            Some(now),
            false,
        )
        .await;

        assert!(
            !skip,
            "Multi-volume archive part of same size must re-upload if local file is newer"
        );
    }

    #[tokio::test]
    async fn test_should_skip_upload_skips_when_hash_matches() {
        let dir = tempfile::tempdir().unwrap();
        let local_file = dir.path().join("backup.part01.rar");
        tokio::fs::write(&local_file, b"IDENTICAL_CONTENT").await.unwrap();
        let local_size = 17i64;
        let local_md5 = compute_md5_file(&local_file).await.unwrap();

        let mut files = HashMap::new();
        files.insert(
            "/Backups/backup.part01.rar".to_string(),
            RemoteFileInfo {
                name: "backup.part01.rar".to_string(),
                path: "/Backups/backup.part01.rar".to_string(),
                is_dir: false,
                size: local_size,
                md5: Some(local_md5),
                etag: None,
                last_modified: None,
            },
        );

        let provider = FakeProvider {
            files: Mutex::new(files),
        };

        let skip = should_skip_upload(
            &provider,
            &local_file,
            "/Backups/backup.part01.rar",
            local_size,
            None,
            false,
        )
        .await;

        assert!(skip, "Should skip when MD5 matches");
    }
}
