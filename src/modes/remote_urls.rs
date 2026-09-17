use crate::client::{build_http_client, retry_request, stream_download_to_file};
use crate::storage::{RemoteFileInfo, StorageProvider};
use crate::utils::hash::compute_md5_file;
use anyhow::{bail, Context, Result};
use regex::Regex;
use reqwest::header::{AUTHORIZATION, USER_AGENT};
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};
use url::Url;

#[derive(Debug, Clone)]
pub struct DownloadTask {
    pub url: String,
    pub subfolder: String,
    pub tag: String,
    pub asset_base_name: String,
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

async fn read_local_urls(path: &std::path::Path) -> Result<Vec<String>> {
    let content = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read local URLs file: {}", path.display()))?;
    Ok(content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect())
}

pub async fn run_remote_urls(
    provider: &dyn StorageProvider,
    remote_base_dir: &str,
    urls_file_override: Option<&std::path::Path>,
) -> Result<()> {
    let clean_base = if remote_base_dir.is_empty() || remote_base_dir == "/" {
        "/Upload".to_string()
    } else {
        format!("/{}", remote_base_dir.trim_matches('/'))
    };

    // 1. Resolve URLs list source (explicit path -> local next to exe/cwd -> cloud)
    let urls =
        if let Some(custom_path) = urls_file_override {
            if custom_path.exists() {
                info!(
                    "Reading URLs from local specified file: '{}'...",
                    custom_path.display()
                );
                read_local_urls(custom_path).await?
            } else {
                let remote_path = custom_path.to_string_lossy().replace('\\', "/");
                info!("Fetching URLs from remote cloud path: '{}'...", remote_path);
                provider.read_text_file(&remote_path).await?
            }
        } else {
            // Check local candidates: ./urls.txt or next to binary
            let mut local_found = None;
            let cwd_candidate = std::path::PathBuf::from("urls.txt");
            if cwd_candidate.exists() {
                local_found = Some(cwd_candidate);
            } else if let Ok(exe_path) = env::current_exe() {
                if let Some(exe_dir) = exe_path.parent() {
                    let exe_candidate = exe_dir.join("urls.txt");
                    if exe_candidate.exists() {
                        local_found = Some(exe_candidate);
                    }
                }
            }

            if let Some(local_path) = local_found {
                info!(
                    "Found local urls.txt at: '{}'. Reading URLs locally...",
                    local_path.display()
                );
                read_local_urls(&local_path).await?
            } else {
                let remote_urls_file = format!("{}/urls.txt", clean_base);
                info!(
                    "Local urls.txt not found. Fetching URLs list from cloud: '{}'...",
                    remote_urls_file
                );

                match provider.read_text_file(&remote_urls_file).await {
                    Ok(lines) => lines,
                    Err(e) => {
                        let fallback_file = "/urls.txt";
                        match provider.read_text_file(fallback_file).await {
                            Ok(lines) => {
                                info!("Found urls.txt in root cloud folder '{}'", fallback_file);
                                lines
                            }
                            Err(_) => {
                                bail!(
                                "urls.txt not found locally (in current dir or next to binary) \
                                 and failed to read from cloud ('{}' and '{}'): {}\n\
                                 Create urls.txt locally or upload it to your cloud folder.",
                                remote_urls_file, fallback_file, e
                            );
                            }
                        }
                    }
                }
            }
        };

    if urls.is_empty() {
        info!("No URLs found to download (urls.txt is empty). Exiting.");
        return Ok(());
    }

    info!("Found {} URL(s) to process.", urls.len());

    let tmp_dir = PathBuf::from("tmp_downloads");
    tokio::fs::create_dir_all(&tmp_dir)
        .await
        .context("Failed to create temporary download directory")?;

    let http_client = build_http_client(Some(Duration::from_secs(1800)))?;

    // Cache of remote directory listings to avoid repeated list_dir requests
    let mut dir_cache: HashMap<String, Vec<RemoteFileInfo>> = HashMap::new();

    for raw_line in urls {
        let raw_url = raw_line.trim();
        if raw_url.is_empty() || raw_url.starts_with('#') {
            continue;
        }

        info!("Processing URL: {}", raw_url);

        let tasks = match resolve_download_urls(&http_client, raw_url).await {
            Ok(t) => t,
            Err(err) => {
                warn!("  [!] Failed to resolve '{}': {}", raw_url, err);
                continue;
            }
        };

        for task in tasks {
            let target_remote_dir = if task.subfolder.is_empty() {
                clean_base.clone()
            } else {
                format!("{}/{}", clean_base, task.subfolder.trim_matches('/'))
            };

            let _ = provider.ensure_dir(&target_remote_dir).await;

            let file_name = if task.tag.is_empty() {
                task.asset_base_name.clone()
            } else {
                format!("[{}] {}", task.tag, task.asset_base_name)
            };

            let remote_path = format!("{}/{}", target_remote_dir, file_name);

            // 1. Safe cleanup of older versions for tagged releases
            if !task.tag.is_empty() {
                let dir_items = match dir_cache.get(&target_remote_dir) {
                    Some(items) => items.clone(),
                    None => {
                        let fetched = provider
                            .list_dir(&target_remote_dir)
                            .await
                            .unwrap_or_default();
                        dir_cache.insert(target_remote_dir.clone(), fetched.clone());
                        fetched
                    }
                };

                let (already_exists, remaining_items) =
                    safe_clean_old_versions(provider, &task.tag, &task.asset_base_name, dir_items)
                        .await;

                // Update cache with remaining items
                dir_cache.insert(target_remote_dir.clone(), remaining_items);

                if already_exists {
                    info!(
                        "  [*] File '{}' already exists in {}. Skipping download.",
                        file_name, target_remote_dir
                    );
                    continue;
                }
            }

            // 2. Download to local temp directory
            let local_dest = tmp_dir.join(&file_name);
            info!("  [*] Downloading {} -> {}", task.url, local_dest.display());

            let dl_res = retry_request("download_file", || http_client.get(&task.url).send()).await;
            let resp = match dl_res {
                Ok(r) => r,
                Err(err) => {
                    warn!("  [!] Failed to download '{}': {}", task.url, err);
                    continue;
                }
            };

            if let Err(err) = stream_download_to_file(resp, &local_dest).await {
                warn!(
                    "  [!] Error saving download to '{}': {}",
                    local_dest.display(),
                    err
                );
                let _ = tokio::fs::remove_file(&local_dest).await;
                continue;
            }

            // 3. For raw files without a tag, check if remote MD5 matches local MD5
            if task.tag.is_empty() {
                if let Ok(local_md5) = compute_md5_file(&local_dest).await {
                    if let Ok(Some(remote_info)) = provider.get_file_info(&remote_path).await {
                        if let Some(ref remote_md5) = remote_info.md5 {
                            if local_md5.eq_ignore_ascii_case(remote_md5) {
                                info!(
                                    "  [*] Remote file '{}' has identical MD5. Skipping upload.",
                                    remote_path
                                );
                                let _ = tokio::fs::remove_file(&local_dest).await;
                                continue;
                            }
                        }
                    }
                }
            }

            info!("  [*] Uploading '{}' to '{}'...", file_name, remote_path);

            match provider.upload_file(&local_dest, &remote_path).await {
                Ok(_) => {
                    info!("  [+] Successfully uploaded: {}", remote_path);
                    // Invalidate directory cache for this folder so new uploads reflect in subsequent checks
                    dir_cache.remove(&target_remote_dir);
                }
                Err(e) => {
                    warn!("  [!] Failed to upload '{}': {}", remote_path, e);
                }
            }

            let _ = tokio::fs::remove_file(&local_dest).await;
        }
    }

    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
    info!("All URL mirror tasks completed.");
    Ok(())
}

async fn resolve_download_urls(client: &Client, raw_url: &str) -> Result<Vec<DownloadTask>> {
    let parsed = Url::parse(raw_url).context("Failed to parse URL")?;

    if parsed.host_str() == Some("github.com") {
        let segments: Vec<&str> = parsed
            .path()
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();

        if segments.len() >= 2 {
            let owner = segments[0];
            let repo = segments[1];

            let is_direct = segments
                .iter()
                .any(|&s| s == "download" || s == "archive" || s == "raw");

            if !is_direct {
                let api_url = format!(
                    "https://api.github.com/repos/{}/{}/releases/latest",
                    owner, repo
                );
                info!(
                    "  [*] Resolving GitHub latest release for {}/{}...",
                    owner, repo
                );

                let (asset_tasks, _tag) =
                    fetch_latest_github_assets(client, &api_url, repo).await?;
                return Ok(asset_tasks);
            }
        }
    }

    let file_name = get_file_name_from_url(raw_url);
    Ok(vec![DownloadTask {
        url: raw_url.to_string(),
        subfolder: "".to_string(),
        tag: "".to_string(),
        asset_base_name: file_name,
    }])
}

async fn fetch_latest_github_assets(
    client: &Client,
    api_url: &str,
    repo_name: &str,
) -> Result<(Vec<DownloadTask>, String)> {
    let mut req = client.get(api_url);

    // Set User-Agent as required by GitHub API
    req = req.header(
        USER_AGENT,
        "cloud-uploader/1.0 (+https://github.com/cloud-uploader)",
    );

    // Support GITHUB_TOKEN for 5000 req/hour rate limit
    if let Ok(gh_token) = env::var("GITHUB_TOKEN") {
        let token_clean = gh_token.trim();
        if !token_clean.is_empty() {
            req = req.header(AUTHORIZATION, format!("Bearer {}", token_clean));
        }
    }

    let res = retry_request("github_api_releases", || req.try_clone().unwrap().send())
        .await
        .with_context(|| format!("Failed to request GitHub API: {}", api_url))?;

    if !res.status().is_success() {
        let status = res.status();
        let err_text = res.text().await.unwrap_or_default();
        bail!("GitHub API returned HTTP {}: {}", status, err_text);
    }

    let release: GhRelease = res
        .json()
        .await
        .context("Failed to parse GitHub release JSON")?;

    let mut tasks = Vec::new();
    for asset in release.assets {
        let lower = asset.name.to_lowercase();
        if lower.ends_with(".apk")
            || lower.ends_with(".exe")
            || lower.ends_with(".zip")
            || lower.ends_with(".tar.gz")
        {
            tasks.push(DownloadTask {
                url: asset.browser_download_url,
                subfolder: repo_name.to_string(),
                tag: release.tag_name.clone(),
                asset_base_name: asset.name,
            });
        }
    }

    if tasks.is_empty() {
        bail!(
            "No matching release assets (.apk, .exe, .zip, .tar.gz) found in latest release of {}",
            repo_name
        );
    }

    Ok((tasks, release.tag_name))
}

/// Safely removes old versions of a specific asset from remote folder.
/// Only deletes files strictly matching: `^\[(.+)\]\s*{escaped_asset_base_name}$`
/// where the tag does not match current_tag.
/// Returns: (already_exists: bool, remaining_items: Vec<RemoteFileInfo>)
pub async fn safe_clean_old_versions(
    provider: &dyn StorageProvider,
    current_tag: &str,
    asset_base_name: &str,
    items: Vec<RemoteFileInfo>,
) -> (bool, Vec<RemoteFileInfo>) {
    let current_file_name = format!("[{}] {}", current_tag, asset_base_name);
    let mut already_exists = false;
    let mut remaining = Vec::new();

    // Regex strictly matching: ^\[([^\]]+)\]\s*(.+)$
    let re = match Regex::new(r"^\[([^\]]+)\]\s*(.+)$") {
        Ok(r) => r,
        Err(_) => return (false, items),
    };

    for item in items {
        if item.is_dir {
            remaining.push(item);
            continue;
        }

        if item.name == current_file_name {
            already_exists = true;
            remaining.push(item);
            continue;
        }

        if let Some(caps) = re.captures(&item.name) {
            let item_tag = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let item_asset_name = caps.get(2).map(|m| m.as_str()).unwrap_or("");

            // STRICT MATCH: Only delete if the asset base name matches exactly
            if item_asset_name == asset_base_name && item_tag != current_tag {
                info!(
                    "  [*] Found older version '{}' of '{}'. Deleting...",
                    item.name, asset_base_name
                );
                let _ = provider.delete_file(&item.path).await;
                continue; // Do not include in remaining
            }
        }

        remaining.push(item);
    }

    (already_exists, remaining)
}

fn get_file_name_from_url(raw_url: &str) -> String {
    if let Ok(u) = Url::parse(raw_url) {
        if let Some(seg) = u.path_segments().and_then(|mut s| s.next_back()) {
            if !seg.is_empty() && seg != "/" {
                return seg.to_string();
            }
        }
    }
    "downloaded_file.bin".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regex_matching_protects_other_files() {
        let re = Regex::new(r"^\[([^\]]+)\]\s*(.+)$").unwrap();

        // Should match valid release tags
        let caps = re.captures("[v1.2.3] myapp.apk").unwrap();
        assert_eq!(&caps[1], "v1.2.3");
        assert_eq!(&caps[2], "myapp.apk");

        // Should match user file with bracket prefix, but asset name is "notes.txt"
        let caps_user = re.captures("[черновик] notes.txt").unwrap();
        assert_eq!(&caps_user[1], "черновик");
        assert_eq!(&caps_user[2], "notes.txt");

        // When updating "myapp.apk", "notes.txt" does not match asset_base_name!
        assert_ne!(&caps_user[2], "myapp.apk");
    }
}
