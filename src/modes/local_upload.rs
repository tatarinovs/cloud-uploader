use super::Stats;
use crate::storage::StorageProvider;
use crate::utils::format::{format_bytes, format_rate};
use crate::utils::hash::compute_md5_file;
use crate::utils::path::{join_remote, normalize_remote_dir};
use anyhow::{Context, Result};
use futures_util::stream::{self, StreamExt};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};
use tracing::{error, info, warn};
use walkdir::WalkDir;

pub struct UploadOptions {
    pub overwrite: bool,
    pub dry_run: bool,
    pub jobs: usize,
}

struct FileJob {
    local: PathBuf,
    remote: String,
    size: u64,
    modified: Option<SystemTime>,
}

enum Outcome {
    Uploaded(u64),
    Skipped,
    Failed,
}

/// Uploads a file, or the contents of a directory recursively, into `remote_dir`.
pub async fn run_local_upload(
    provider: &dyn StorageProvider,
    local_path: &Path,
    remote_dir: &str,
    opts: &UploadOptions,
) -> Result<Stats> {
    let remote_dir = normalize_remote_dir(remote_dir);
    let meta = std::fs::metadata(local_path)
        .with_context(|| format!("Cannot access '{}'", local_path.display()))?;
    let mut stats = Stats::default();

    let jobs = if meta.is_file() {
        let name = local_path
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("'{}' has no valid UTF-8 file name", local_path.display()))?;
        vec![FileJob {
            local: local_path.to_path_buf(),
            remote: join_remote(&remote_dir, name),
            size: meta.len(),
            modified: meta.modified().ok(),
        }]
    } else {
        info!("Scanning '{}'...", local_path.display());
        let scan = scan_directory(local_path, &remote_dir);
        stats.failed += scan.errors;
        for dir in &scan.dirs {
            if opts.dry_run {
                continue;
            }
            if let Err(err) = provider.ensure_dir(dir).await {
                error!("  [!] Failed to create remote folder '{dir}': {err:#}");
                stats.failed += 1;
            }
        }
        scan.files
    };

    let total_bytes: u64 = jobs.iter().map(|j| j.size).sum();
    info!(
        "Found {} file(s), {} total; uploading into '{remote_dir}' with {} parallel job(s)",
        jobs.len(),
        format_bytes(total_bytes),
        opts.jobs
    );

    let started = Instant::now();
    let mut results = stream::iter(jobs)
        .map(|job| process_file(provider, job, opts))
        .buffer_unordered(opts.jobs.max(1));
    while let Some(outcome) = results.next().await {
        stats.processed += 1;
        match outcome {
            Outcome::Uploaded(bytes) => {
                stats.uploaded += 1;
                stats.bytes_uploaded += bytes;
            }
            Outcome::Skipped => stats.skipped += 1,
            Outcome::Failed => stats.failed += 1,
        }
    }

    info!("--------------------------------------------------");
    info!(
        "Files: {} processed, {} uploaded ({}), {} unchanged, {} failed",
        stats.processed,
        stats.uploaded,
        format_bytes(stats.bytes_uploaded),
        stats.skipped,
        stats.failed
    );
    if !opts.dry_run && stats.bytes_uploaded > 0 {
        info!(
            "Elapsed: {:.0}s, average {}",
            started.elapsed().as_secs_f64(),
            format_rate(stats.bytes_uploaded, started.elapsed())
        );
    }
    Ok(stats)
}

struct Scan {
    dirs: Vec<String>,
    files: Vec<FileJob>,
    errors: u64,
}

fn scan_directory(root: &Path, remote_dir: &str) -> Scan {
    let mut scan = Scan {
        dirs: Vec::new(),
        files: Vec::new(),
        errors: 0,
    };
    let walker = WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .min_depth(1);
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                error!(
                    "  [!] Cannot read '{}': {err}",
                    err.path().unwrap_or(root).display()
                );
                scan.errors += 1;
                continue;
            }
        };
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let Some(relative) = relative.to_str() else {
            error!(
                "  [!] Skipping '{}': file name is not valid UTF-8",
                entry.path().display()
            );
            scan.errors += 1;
            continue;
        };
        let remote = join_remote(remote_dir, relative);
        let file_type = entry.file_type();

        if file_type.is_dir() {
            scan.dirs.push(remote);
        } else if file_type.is_file() {
            match entry.metadata() {
                Ok(meta) => scan.files.push(FileJob {
                    local: entry.path().to_path_buf(),
                    remote,
                    size: meta.len(),
                    modified: meta.modified().ok(),
                }),
                Err(err) => {
                    error!("  [!] Cannot read '{}': {err}", entry.path().display());
                    scan.errors += 1;
                }
            }
        } else {
            warn!(
                "  [~] Skipping symlink or special file '{}'",
                entry.path().display()
            );
        }
    }
    scan
}

async fn process_file(
    provider: &dyn StorageProvider,
    job: FileJob,
    opts: &UploadOptions,
) -> Outcome {
    if !opts.overwrite {
        match is_up_to_date(provider, &job).await {
            Ok(true) => {
                info!("  [=] Unchanged: {}", job.remote);
                return Outcome::Skipped;
            }
            Ok(false) => {}
            Err(err) => warn!(
                "  [~] Cannot check remote '{}' ({err:#}); uploading anyway",
                job.remote
            ),
        }
    }

    if opts.dry_run {
        info!(
            "  [dry-run] Would upload {} ({})",
            job.remote,
            format_bytes(job.size)
        );
        return Outcome::Uploaded(job.size);
    }

    info!(
        "  [^] Uploading {} ({})...",
        job.remote,
        format_bytes(job.size)
    );
    let started = Instant::now();
    match provider.upload_file(&job.local, &job.remote).await {
        Ok(()) => {
            info!(
                "  [+] Uploaded {} in {:.1}s ({})",
                job.remote,
                started.elapsed().as_secs_f64(),
                format_rate(job.size, started.elapsed())
            );
            Outcome::Uploaded(job.size)
        }
        Err(err) => {
            error!("  [!] Failed to upload '{}': {err:#}", job.remote);
            Outcome::Failed
        }
    }
}

/// Decides whether the remote copy already matches the local file:
/// 1. different size -> upload;
/// 2. provider has a real MD5 -> compare hashes;
/// 3. otherwise local mtime newer than remote (2s tolerance) -> upload. This
///    matters for multi-volume archives whose parts all have the same size.
async fn is_up_to_date(provider: &dyn StorageProvider, job: &FileJob) -> Result<bool> {
    let Some(remote) = provider.get_file_info(&job.remote).await? else {
        return Ok(false);
    };
    if remote.is_dir || remote.size != job.size {
        return Ok(false);
    }

    if let Some(remote_md5) = remote.md5.as_deref() {
        let local_md5 = compute_md5_file(&job.local).await?;
        if !local_md5.eq_ignore_ascii_case(remote_md5) {
            info!("  [~] Same size but different MD5: {}", job.remote);
            return Ok(false);
        }
        return Ok(true);
    }

    if let (Some(local), Some(remote_mtime)) = (job.modified, remote.modified) {
        let local: chrono::DateTime<chrono::Utc> = local.into();
        if local > remote_mtime + chrono::Duration::seconds(2) {
            info!(
                "  [~] Same size but local file is newer: {} (local {}, remote {})",
                job.remote,
                local.format("%Y-%m-%d %H:%M:%S UTC"),
                remote_mtime.format("%Y-%m-%d %H:%M:%S UTC")
            );
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::RemoteFileInfo;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeProvider {
        files: Mutex<HashMap<String, RemoteFileInfo>>,
        uploads: Mutex<Vec<String>>,
        dirs: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl StorageProvider for FakeProvider {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn ensure_dir(&self, remote_dir: &str) -> Result<()> {
            self.dirs.lock().unwrap().push(remote_dir.to_string());
            Ok(())
        }
        async fn get_file_info(&self, remote_path: &str) -> Result<Option<RemoteFileInfo>> {
            Ok(self.files.lock().unwrap().get(remote_path).cloned())
        }
        async fn upload_file(&self, _local_path: &Path, remote_path: &str) -> Result<()> {
            self.uploads.lock().unwrap().push(remote_path.to_string());
            Ok(())
        }
        async fn delete(&self, remote_path: &str, _is_dir: bool) -> Result<()> {
            self.files.lock().unwrap().remove(remote_path);
            Ok(())
        }
        async fn read_text_file(&self, _remote_path: &str) -> Result<Option<String>> {
            Ok(None)
        }
        async fn list_dir(&self, _remote_dir: &str) -> Result<Vec<RemoteFileInfo>> {
            Ok(Vec::new())
        }
    }

    fn remote(
        path: &str,
        size: u64,
        md5: Option<String>,
        modified: Option<chrono::DateTime<chrono::Utc>>,
    ) -> RemoteFileInfo {
        RemoteFileInfo {
            name: path.rsplit('/').next().unwrap().to_string(),
            path: path.to_string(),
            is_dir: false,
            size,
            md5,
            modified,
        }
    }

    async fn job_for(dir: &Path, content: &[u8], modified: Option<SystemTime>) -> FileJob {
        let local = dir.join("backup.part01.rar");
        tokio::fs::write(&local, content).await.unwrap();
        FileJob {
            local,
            remote: "/Backups/backup.part01.rar".into(),
            size: content.len() as u64,
            modified,
        }
    }

    #[tokio::test]
    async fn reuploads_on_md5_mismatch_with_same_size() {
        let dir = tempfile::tempdir().unwrap();
        let job = job_for(dir.path(), b"NEW_DATA_CONTENT", None).await;
        let provider = FakeProvider::default();
        provider.files.lock().unwrap().insert(
            job.remote.clone(),
            remote(
                &job.remote,
                16,
                Some("0123456789abcdef0123456789abcdef".into()),
                None,
            ),
        );
        assert!(!is_up_to_date(&provider, &job).await.unwrap());
    }

    #[tokio::test]
    async fn skips_when_md5_matches() {
        let dir = tempfile::tempdir().unwrap();
        let job = job_for(dir.path(), b"IDENTICAL_CONTENT", None).await;
        let md5 = compute_md5_file(&job.local).await.unwrap();
        let provider = FakeProvider::default();
        provider
            .files
            .lock()
            .unwrap()
            .insert(job.remote.clone(), remote(&job.remote, 17, Some(md5), None));
        assert!(is_up_to_date(&provider, &job).await.unwrap());
    }

    #[tokio::test]
    async fn reuploads_newer_multivolume_part_without_md5() {
        let dir = tempfile::tempdir().unwrap();
        let job = job_for(dir.path(), b"SAME_SIZE_BYTES!", Some(SystemTime::now())).await;
        let provider = FakeProvider::default();
        let yesterday = chrono::Utc::now() - chrono::Duration::hours(24);
        provider.files.lock().unwrap().insert(
            job.remote.clone(),
            remote(&job.remote, 16, None, Some(yesterday)),
        );
        assert!(!is_up_to_date(&provider, &job).await.unwrap());
    }

    #[tokio::test]
    async fn uploads_directory_tree_and_skips_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub/deeper")).unwrap();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        std::fs::write(dir.path().join("sub/deeper/b c.txt"), b"bb").unwrap();
        let md5 = compute_md5_file(&dir.path().join("a.txt")).await.unwrap();

        let provider = FakeProvider::default();
        provider.files.lock().unwrap().insert(
            "/Backups/a.txt".into(),
            remote("/Backups/a.txt", 1, Some(md5), None),
        );

        let opts = UploadOptions {
            overwrite: false,
            dry_run: false,
            jobs: 2,
        };
        let stats = run_local_upload(&provider, dir.path(), "/Backups/", &opts)
            .await
            .unwrap();

        assert_eq!(stats.processed, 2);
        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.uploaded, 1);
        assert_eq!(stats.failed, 0);
        assert_eq!(
            *provider.uploads.lock().unwrap(),
            vec!["/Backups/sub/deeper/b c.txt"]
        );
        assert_eq!(
            *provider.dirs.lock().unwrap(),
            vec!["/Backups/sub", "/Backups/sub/deeper"]
        );
    }

    #[tokio::test]
    async fn dry_run_uploads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        let provider = FakeProvider::default();
        let opts = UploadOptions {
            overwrite: true,
            dry_run: true,
            jobs: 1,
        };
        let stats = run_local_upload(&provider, dir.path(), "/x", &opts)
            .await
            .unwrap();
        assert_eq!(stats.uploaded, 1);
        assert!(provider.uploads.lock().unwrap().is_empty());
    }
}
