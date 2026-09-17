use crate::storage::StorageProvider;
use crate::utils::format::format_bytes;
use crate::utils::hash::compute_md5_file;
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

    let clean_base = if remote_base_dir.is_empty() || remote_base_dir == "/" {
        "".to_string()
    } else {
        format!("/{}", remote_base_dir.trim_matches('/'))
    };

    if metadata.is_file() {
        // Single file upload
        let file_name = local_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("uploaded_file");
        let remote_file_path = format!("{}/{}", clean_base, file_name);

        info!(
            "Uploading single file '{}' -> '{}' ({})",
            local_path.display(),
            remote_file_path,
            format_bytes(metadata.len() as i64)
        );

        if should_skip_upload(
            provider,
            local_path,
            &remote_file_path,
            metadata.len() as i64,
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

        // Convert Windows separators to URL slashes
        let rel_str = rel_path.to_string_lossy().replace('\\', "/");
        let remote_target = if clean_base.is_empty() {
            format!("/{}", rel_str)
        } else {
            format!("{}/{}", clean_base, rel_str)
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
        total_files += 1;
        total_bytes += file_size;

        // Ensure parent remote directory exists
        let parent_remote = match remote_target.rsplit_once('/') {
            Some((p, _)) if !p.is_empty() => p.to_string(),
            _ => "/".to_string(),
        };

        if !created_dirs.contains(&parent_remote)
            && provider.ensure_dir(&parent_remote).await.is_ok()
        {
            created_dirs.insert(parent_remote);
        }

        if should_skip_upload(
            provider,
            current_path,
            &remote_target,
            file_size,
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

async fn should_skip_upload(
    provider: &dyn StorageProvider,
    local_path: &Path,
    remote_path: &str,
    local_size: i64,
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

    // 3. Remote file exists and has the EXACT same size (remote_info.size == local_size).
    // If the storage protocol does not provide an MD5 hash (like WebDAV without ETag MD5),
    // treat identical size as already uploaded to avoid redundant re-uploads.
    true
}
