//! Mirrors files from a list of URLs (typically GitHub releases) into the cloud.

use super::Stats;
use crate::client::{
    build_http_client, download_to_file, http_error, send, API_TIMEOUT, USER_AGENT,
};
use crate::storage::{RemoteFileInfo, StorageProvider};
use crate::utils::format::format_bytes;
use crate::utils::hash::compute_md5_file;
use crate::utils::path::{encode_component, join_remote, normalize_remote_dir, sanitize_file_name};
use crate::utils::text::parse_list;
use anyhow::{bail, Context, Result};
use percent_encoding::percent_decode_str;
use regex::Regex;
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT as USER_AGENT_HEADER};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use tracing::{error, info, warn};
use url::Url;

pub const DEFAULT_ASSET_FILTER: &str = r"(?i)\.(apk|exe|zip|tar\.gz)$";
const URLS_FILE: &str = "urls.txt";

/// Matches managed release files: `[tag] asset-name`.
static VERSIONED_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[([^\]]+)\]\s*(.+)$").expect("valid regex"));

pub struct MirrorOptions {
    pub dry_run: bool,
    pub asset_filter: Regex,
}

pub enum UrlSource {
    Local(PathBuf),
    Remote(String),
}

pub struct UrlList {
    pub urls: Vec<String>,
    pub source: UrlSource,
}

impl UrlList {
    /// The cloud path of the list file, so that `--clean` can keep it.
    pub fn remote_path(&self) -> Option<&str> {
        match &self.source {
            UrlSource::Remote(path) => Some(path),
            UrlSource::Local(_) => None,
        }
    }

    fn describe(&self) -> String {
        match &self.source {
            UrlSource::Local(path) => format!("local file '{}'", path.display()),
            UrlSource::Remote(path) => format!("cloud file '{path}'"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct DownloadTask {
    url: String,
    /// Target subfolder (repository name) or empty for the base folder.
    subfolder: String,
    /// Release tag; tagged files are stored as `[tag] asset`.
    tag: Option<String>,
    asset: String,
}

impl DownloadTask {
    fn remote_name(&self) -> String {
        match &self.tag {
            Some(tag) => format!("[{tag}] {}", self.asset),
            None => self.asset.clone(),
        }
    }
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, PartialEq)]
struct GithubRelease {
    owner: String,
    repo: String,
    /// `None` means the latest release.
    tag: Option<String>,
}

/// Finds the URL list: `--urls-file` (local, then cloud), otherwise
/// `./urls.txt`, `<exe dir>/urls.txt`, `<remote dir>/urls.txt`, `/urls.txt`.
pub async fn load_url_list(
    provider: &dyn StorageProvider,
    remote_dir: &str,
    explicit: Option<&Path>,
) -> Result<UrlList> {
    if let Some(path) = explicit {
        if path.is_file() {
            return read_local_list(path);
        }
        let remote = normalize_remote_dir(&path.to_string_lossy());
        return match provider.read_text_file(&remote).await? {
            Some(text) => Ok(UrlList {
                urls: parse_list(&text),
                source: UrlSource::Remote(remote),
            }),
            None => bail!(
                "URL list '{}' not found locally or in the cloud",
                path.display()
            ),
        };
    }

    let mut local_candidates = vec![PathBuf::from(URLS_FILE)];
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        local_candidates.push(exe_dir.join(URLS_FILE));
    }
    if let Some(found) = local_candidates.iter().find(|p| p.is_file()) {
        return read_local_list(found);
    }

    let mut remote_candidates = vec![join_remote(remote_dir, URLS_FILE)];
    let root_file = format!("/{URLS_FILE}");
    if !remote_candidates.contains(&root_file) {
        remote_candidates.push(root_file);
    }
    for remote in &remote_candidates {
        if let Some(text) = provider.read_text_file(remote).await? {
            return Ok(UrlList {
                urls: parse_list(&text),
                source: UrlSource::Remote(remote.clone()),
            });
        }
    }

    bail!(
        "urls.txt not found. Looked in the current folder, next to the executable and in the cloud ({}). \
         Create it or pass --urls-file",
        remote_candidates.join(", ")
    )
}

fn read_local_list(path: &Path) -> Result<UrlList> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read '{}'", path.display()))?;
    Ok(UrlList {
        urls: parse_list(&text),
        source: UrlSource::Local(path.to_path_buf()),
    })
}

pub async fn run_remote_urls(
    provider: &dyn StorageProvider,
    remote_dir: &str,
    list: &UrlList,
    opts: &MirrorOptions,
) -> Result<Stats> {
    let mut stats = Stats::default();
    if list.urls.is_empty() {
        info!("The URL list ({}) is empty, nothing to do", list.describe());
        return Ok(stats);
    }
    info!(
        "Mirroring {} URL(s) from {}",
        list.urls.len(),
        list.describe()
    );

    let mut mirror = Mirror {
        provider,
        base_dir: normalize_remote_dir(remote_dir),
        opts,
        client: build_http_client()?,
        temp_dir: tempfile::Builder::new()
            .prefix("cloud-uploader-")
            .tempdir()
            .context("Failed to create a temporary folder")?,
        listings: HashMap::new(),
        ensured_dirs: HashSet::new(),
    };

    for url in &list.urls {
        info!("Processing {url}");
        let tasks = match resolve_tasks(&mirror.client, url, &opts.asset_filter).await {
            Ok(tasks) => tasks,
            Err(err) => {
                error!("  [!] {err:#}");
                stats.failed += 1;
                continue;
            }
        };
        for task in tasks {
            stats.processed += 1;
            match mirror.mirror(&task, &mut stats).await {
                Ok(Some(bytes)) => {
                    stats.uploaded += 1;
                    stats.bytes_uploaded += bytes;
                }
                Ok(None) => stats.skipped += 1,
                Err(err) => {
                    error!("  [!] {}: {err:#}", task.remote_name());
                    stats.failed += 1;
                }
            }
        }
    }

    info!("--------------------------------------------------");
    info!(
        "Files: {} processed, {} uploaded ({}), {} up to date, {} old versions removed, {} failed",
        stats.processed,
        stats.uploaded,
        format_bytes(stats.bytes_uploaded),
        stats.skipped,
        stats.deleted,
        stats.failed
    );
    Ok(stats)
}

struct Mirror<'a> {
    provider: &'a dyn StorageProvider,
    base_dir: String,
    opts: &'a MirrorOptions,
    client: Client,
    temp_dir: tempfile::TempDir,
    /// Cached folder listings, kept in sync with our own changes.
    listings: HashMap<String, Vec<RemoteFileInfo>>,
    ensured_dirs: HashSet<String>,
}

impl Mirror<'_> {
    /// Returns `Some(bytes)` if a file was uploaded, `None` if it was up to date.
    async fn mirror(&mut self, task: &DownloadTask, stats: &mut Stats) -> Result<Option<u64>> {
        let target_dir = if task.subfolder.is_empty() {
            self.base_dir.clone()
        } else {
            join_remote(&self.base_dir, &task.subfolder)
        };
        let file_name = task.remote_name();
        let remote_path = join_remote(&target_dir, &file_name);

        if let Some(tag) = &task.tag {
            let exists = self
                .listing(&target_dir)
                .await?
                .iter()
                .any(|item| !item.is_dir && item.name == file_name);
            if exists {
                info!("  [=] Up to date: {remote_path}");
                self.remove_old_versions(&target_dir, tag, &task.asset, stats)
                    .await;
                return Ok(None);
            }
        }

        if self.opts.dry_run {
            info!("  [dry-run] Would download {} -> {remote_path}", task.url);
            if let Some(tag) = &task.tag {
                self.remove_old_versions(&target_dir, tag, &task.asset, stats)
                    .await;
            }
            return Ok(Some(0));
        }

        let local = self.temp_dir.path().join(&file_name);
        info!("  [v] Downloading {}", task.url);
        let size = download_to_file(&self.client, &task.url, &local).await?;
        let result = self
            .upload(task, &local, size, &target_dir, &remote_path)
            .await;
        let _ = tokio::fs::remove_file(&local).await;
        let uploaded = result?;

        // Old versions are removed only after the new one is safely stored.
        if let (Some(tag), true) = (&task.tag, uploaded) {
            self.remove_old_versions(&target_dir, tag, &task.asset, stats)
                .await;
        }
        Ok(uploaded.then_some(size))
    }

    async fn upload(
        &mut self,
        task: &DownloadTask,
        local: &Path,
        size: u64,
        target_dir: &str,
        remote_path: &str,
    ) -> Result<bool> {
        if task.tag.is_none() {
            if let Some(remote) = self.provider.get_file_info(remote_path).await? {
                if let Some(remote_md5) = remote.md5.as_deref().filter(|_| remote.size == size) {
                    if compute_md5_file(local)
                        .await?
                        .eq_ignore_ascii_case(remote_md5)
                    {
                        info!("  [=] Unchanged (same MD5): {remote_path}");
                        return Ok(false);
                    }
                }
            }
        }

        if self.ensured_dirs.insert(target_dir.to_string()) {
            if let Err(err) = self.provider.ensure_dir(target_dir).await {
                self.ensured_dirs.remove(target_dir);
                return Err(err.context(format!("Failed to create folder '{target_dir}'")));
            }
        }

        info!("  [^] Uploading {remote_path} ({})", format_bytes(size));
        self.provider.upload_file(local, remote_path).await?;
        info!("  [+] Uploaded {remote_path}");

        if let Some(items) = self.listings.get_mut(target_dir) {
            let name = remote_path
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string();
            items.retain(|item| item.name != name);
            items.push(RemoteFileInfo {
                name,
                path: remote_path.to_string(),
                is_dir: false,
                size,
                md5: None,
                modified: None,
            });
        }
        Ok(true)
    }

    async fn listing(&mut self, dir: &str) -> Result<&mut Vec<RemoteFileInfo>> {
        if !self.listings.contains_key(dir) {
            let items = self
                .provider
                .list_dir(dir)
                .await
                .with_context(|| format!("Failed to list '{dir}'"))?;
            self.listings.insert(dir.to_string(), items);
        }
        Ok(self.listings.get_mut(dir).expect("inserted above"))
    }

    /// Deletes `[other-tag] asset` files. Failures are logged, not fatal.
    async fn remove_old_versions(&mut self, dir: &str, tag: &str, asset: &str, stats: &mut Stats) {
        let provider = self.provider;
        let dry_run = self.opts.dry_run;
        let items = match self.listing(dir).await {
            Ok(items) => items,
            Err(err) => {
                warn!("  [~] Cannot look for old versions: {err:#}");
                return;
            }
        };
        let stale: Vec<RemoteFileInfo> = old_versions(items, tag, asset)
            .into_iter()
            .cloned()
            .collect();
        for item in stale {
            if dry_run {
                info!("  [dry-run] Would delete old version {}", item.path);
                stats.deleted += 1;
                continue;
            }
            match provider.delete(&item.path, false).await {
                Ok(()) => {
                    info!("  [-] Deleted old version {}", item.path);
                    items.retain(|i| i.path != item.path);
                    stats.deleted += 1;
                }
                Err(err) => warn!(
                    "  [~] Failed to delete old version '{}': {err:#}",
                    item.path
                ),
            }
        }
    }
}

/// Files named `[tag] asset` for the same asset but a different tag.
/// Anything else in the folder (including other assets) is never touched.
fn old_versions<'a>(
    items: &'a [RemoteFileInfo],
    tag: &str,
    asset: &str,
) -> Vec<&'a RemoteFileInfo> {
    items
        .iter()
        .filter(|item| !item.is_dir)
        .filter(|item| {
            VERSIONED_NAME
                .captures(&item.name)
                .is_some_and(|caps| &caps[2] == asset && &caps[1] != tag)
        })
        .collect()
}

async fn resolve_tasks(
    client: &Client,
    raw_url: &str,
    filter: &Regex,
) -> Result<Vec<DownloadTask>> {
    let url = Url::parse(raw_url).with_context(|| format!("Invalid URL '{raw_url}'"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("Unsupported URL scheme in '{raw_url}'");
    }
    if let Some(release) = github_release(&url) {
        return fetch_github_release(client, &release, filter).await;
    }
    Ok(vec![DownloadTask {
        url: raw_url.to_string(),
        subfolder: String::new(),
        tag: None,
        asset: file_name_from_url(&url),
    }])
}

/// Recognizes `github.com/owner/repo[/releases[/latest|/tag/<tag>]]`.
/// Direct links (`/releases/download/...`, `/archive/...`, `/raw/...`) are not releases.
fn github_release(url: &Url) -> Option<GithubRelease> {
    if !matches!(url.host_str(), Some("github.com" | "www.github.com")) {
        return None;
    }
    let segments: Vec<String> = url
        .path_segments()?
        .filter(|s| !s.is_empty())
        .map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned())
        .collect();
    if segments.len() < 2
        || segments
            .iter()
            .any(|s| matches!(s.as_str(), "download" | "archive" | "raw"))
    {
        return None;
    }
    let tag = match (
        segments.get(2).map(String::as_str),
        segments.get(3).map(String::as_str),
    ) {
        (Some("releases"), Some("tag")) if segments.len() > 4 => Some(segments[4..].join("/")),
        _ => None,
    };
    Some(GithubRelease {
        owner: segments[0].clone(),
        repo: segments[1].trim_end_matches(".git").to_string(),
        tag,
    })
}

async fn fetch_github_release(
    client: &Client,
    release: &GithubRelease,
    filter: &Regex,
) -> Result<Vec<DownloadTask>> {
    let repo_path = format!(
        "{}/{}",
        encode_component(&release.owner),
        encode_component(&release.repo)
    );
    let api_url = match &release.tag {
        Some(tag) => format!(
            "https://api.github.com/repos/{repo_path}/releases/tags/{}",
            encode_component(tag)
        ),
        None => format!("https://api.github.com/repos/{repo_path}/releases/latest"),
    };
    let token = std::env::var("GITHUB_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

    info!(
        "  [*] Looking up {} release of {}/{}",
        release.tag.as_deref().unwrap_or("the latest"),
        release.owner,
        release.repo
    );
    let resp = send("github: get release", || {
        let mut req = client
            .get(&api_url)
            .header(USER_AGENT_HEADER, USER_AGENT)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(API_TIMEOUT);
        if let Some(token) = &token {
            req = req.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        Ok(req)
    })
    .await?;

    match resp.status() {
        status if status.is_success() => {}
        StatusCode::NOT_FOUND => bail!(
            "No release found for {}/{} (repository missing, private, or without releases)",
            release.owner,
            release.repo
        ),
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS if token.is_none() => {
            return Err(http_error(resp, "GitHub API rate limit reached; set GITHUB_TOKEN").await)
        }
        _ => return Err(http_error(resp, "GitHub API request failed").await),
    }

    let gh: GhRelease = resp.json().await.context("Invalid GitHub release JSON")?;
    let tag = sanitize_tag(&gh.tag_name);
    let subfolder = sanitize_file_name(&release.repo);
    let tasks: Vec<DownloadTask> = gh
        .assets
        .into_iter()
        .filter(|asset| filter.is_match(&asset.name))
        .map(|asset| DownloadTask {
            url: asset.browser_download_url,
            subfolder: subfolder.clone(),
            tag: Some(tag.clone()),
            asset: sanitize_file_name(&asset.name),
        })
        .collect();

    if tasks.is_empty() {
        bail!(
            "Release {} of {}/{} has no assets matching '{}'",
            gh.tag_name,
            release.owner,
            release.repo,
            filter.as_str()
        );
    }
    info!(
        "  [*] Release {}: {} matching asset(s)",
        gh.tag_name,
        tasks.len()
    );
    Ok(tasks)
}

/// A tag must be a valid file name fragment and must not break `[tag]` parsing.
fn sanitize_tag(tag: &str) -> String {
    sanitize_file_name(tag).replace(['[', ']'], "_")
}

fn file_name_from_url(url: &Url) -> String {
    url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|s| !s.is_empty())
        .map(|s| sanitize_file_name(&percent_decode_str(s).decode_utf8_lossy()))
        .unwrap_or_else(|| "downloaded_file.bin".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> RemoteFileInfo {
        RemoteFileInfo {
            name: name.to_string(),
            path: format!("/Upload/repo/{name}"),
            is_dir: false,
            size: 1,
            md5: None,
            modified: None,
        }
    }

    #[test]
    fn old_versions_only_match_same_asset() {
        let items = vec![
            file("[v1.0] app.apk"),
            file("[v2.0] app.apk"),
            file("[v1.0] app-arm64.apk"),
            file("[draft] notes.txt"),
            file("app.apk"),
            RemoteFileInfo {
                is_dir: true,
                ..file("[v0.9] app.apk")
            },
        ];
        let stale: Vec<&str> = old_versions(&items, "v2.0", "app.apk")
            .iter()
            .map(|i| i.name.as_str())
            .collect();
        assert_eq!(stale, vec!["[v1.0] app.apk"]);
    }

    #[test]
    fn recognizes_github_release_urls() {
        let parse = |s: &str| github_release(&Url::parse(s).unwrap());
        assert_eq!(
            parse("https://github.com/owner/repo"),
            Some(GithubRelease {
                owner: "owner".into(),
                repo: "repo".into(),
                tag: None
            })
        );
        assert_eq!(
            parse("https://github.com/owner/repo.git/releases/latest").map(|r| r.repo),
            Some("repo".into())
        );
        assert_eq!(
            parse("https://github.com/o/r/releases/tag/release/1.0").and_then(|r| r.tag),
            Some("release/1.0".into())
        );
        assert_eq!(
            parse("https://github.com/o/r/releases/download/v1/a.zip"),
            None
        );
        assert_eq!(
            parse("https://github.com/o/r/archive/refs/heads/main.zip"),
            None
        );
        assert_eq!(parse("https://github.com/o"), None);
        assert_eq!(parse("https://example.com/o/r"), None);
    }

    #[test]
    fn names_are_sanitized() {
        let url = Url::parse("https://example.com/files/my%20file%3A1.zip?x=1").unwrap();
        assert_eq!(file_name_from_url(&url), "my file_1.zip");
        assert_eq!(
            file_name_from_url(&Url::parse("https://example.com/").unwrap()),
            "downloaded_file.bin"
        );
        assert_eq!(sanitize_tag("release/[1.0]"), "release__1.0_");

        let task = DownloadTask {
            url: String::new(),
            subfolder: "repo".into(),
            tag: Some(sanitize_tag("release/1.0")),
            asset: "app.apk".into(),
        };
        let name = task.remote_name();
        assert_eq!(name, "[release_1.0] app.apk");
        let caps = VERSIONED_NAME.captures(&name).unwrap();
        assert_eq!((&caps[1], &caps[2]), ("release_1.0", "app.apk"));
    }

    #[test]
    fn default_asset_filter() {
        let re = Regex::new(DEFAULT_ASSET_FILTER).unwrap();
        assert!(re.is_match("App-v1.APK"));
        assert!(re.is_match("tool.tar.gz"));
        assert!(!re.is_match("checksums.txt"));
    }
}
