use crate::storage::google::{GoogleAuth, GoogleDriveProvider};
use crate::storage::s3::S3Provider;
use crate::storage::webdav::WebDavProvider;
use crate::storage::yandex::YandexDiskProvider;
use crate::storage::StorageProvider;
use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const MAILRU_WEBDAV_URL: &str = "https://webdav.mail.ru";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ProviderKind {
    #[value(alias = "ya", alias = "yadisk")]
    Yandex,
    #[value(alias = "mail", alias = "mail.ru")]
    Mailru,
    #[value(alias = "gdrive", alias = "drive")]
    Google,
    #[value(alias = "nextcloud", alias = "owncloud")]
    Webdav,
    #[value(alias = "r2", alias = "minio")]
    S3,
}

/// Extracts `--env-file` before clap runs, so that values from the file can
/// serve as defaults for `env = "..."` arguments such as CLOUD_PROVIDER.
pub fn env_file_from_args() -> Option<PathBuf> {
    find_env_file_arg(env::args_os().skip(1))
}

fn find_env_file_arg(args: impl Iterator<Item = OsString>) -> Option<PathBuf> {
    let mut args = args;
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if arg == "--env-file" {
            return args.next().map(PathBuf::from);
        }
        if let Some(value) = arg.to_str().and_then(|s| s.strip_prefix("--env-file=")) {
            return Some(PathBuf::from(value));
        }
    }
    None
}

/// Loads `.env` from the explicit path, else from the current directory, else
/// next to the executable. Variables already set in the environment win.
/// Returns the file that was loaded, if any.
pub fn load_environment(explicit: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Some(path) = explicit {
        if !path.is_file() {
            bail!("Environment file not found: {}", path.display());
        }
        dotenvy::from_path(path)
            .with_context(|| format!("Failed to load environment file {}", path.display()))?;
        return Ok(Some(path.to_path_buf()));
    }

    let mut candidates = vec![PathBuf::from(".env")];
    if let Some(dir) = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        candidates.push(dir.join(".env"));
    }
    for candidate in candidates {
        if candidate.is_file() {
            dotenvy::from_path(&candidate).with_context(|| {
                format!("Failed to load environment file {}", candidate.display())
            })?;
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Reads all required variables, reporting every missing one at once.
fn require<const N: usize>(provider: &str, names: [&str; N]) -> Result<[String; N]> {
    let values = names.map(var);
    let missing: Vec<&str> = names
        .iter()
        .zip(&values)
        .filter(|(_, v)| v.is_none())
        .map(|(n, _)| *n)
        .collect();
    if !missing.is_empty() {
        bail!(
            "{provider} is not configured: set {} in .env or the environment (see --list-providers)",
            missing.join(", ")
        );
    }
    Ok(values.map(Option::unwrap_or_default))
}

fn flag(name: &str) -> bool {
    var(name)
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

pub fn init_provider(kind: ProviderKind) -> Result<Box<dyn StorageProvider>> {
    Ok(match kind {
        ProviderKind::Yandex => {
            let [token] = require("Yandex Disk", ["YANDEX_TOKEN"])?;
            Box::new(YandexDiskProvider::new(&token)?)
        }
        ProviderKind::Mailru => {
            let [user, password] = require("Mail.ru Cloud", ["MAILRU_USER", "MAILRU_PASSWORD"])?;
            let url = var("MAILRU_WEBDAV_URL").unwrap_or_else(|| MAILRU_WEBDAV_URL.to_string());
            // Mail.ru ETags are internal revision ids, not MD5 hashes.
            Box::new(WebDavProvider::new(
                "Mail.ru Cloud",
                &url,
                user,
                password,
                false,
            )?)
        }
        ProviderKind::Google => {
            let auth = match (
                var("GOOGLE_CLIENT_ID"),
                var("GOOGLE_CLIENT_SECRET"),
                var("GOOGLE_REFRESH_TOKEN"),
                var("GOOGLE_DRIVE_TOKEN"),
            ) {
                (Some(client_id), Some(client_secret), Some(refresh_token), _) => {
                    GoogleAuth::RefreshToken {
                        client_id,
                        client_secret,
                        refresh_token,
                    }
                }
                (_, _, _, Some(token)) => GoogleAuth::AccessToken(token),
                _ => bail!(
                    "Google Drive is not configured: set GOOGLE_CLIENT_ID, GOOGLE_CLIENT_SECRET and \
                     GOOGLE_REFRESH_TOKEN (recommended), or a short-lived GOOGLE_DRIVE_TOKEN"
                ),
            };
            Box::new(GoogleDriveProvider::new(auth)?)
        }
        ProviderKind::Webdav => {
            let [url, user, password] =
                require("WebDAV", ["WEBDAV_URL", "WEBDAV_USER", "WEBDAV_PASSWORD"])?;
            Box::new(WebDavProvider::new(
                "WebDAV",
                &url,
                user,
                password,
                flag("WEBDAV_ETAG_IS_MD5"),
            )?)
        }
        ProviderKind::S3 => {
            let [endpoint, bucket, access_key, secret_key] = require(
                "S3",
                [
                    "S3_ENDPOINT",
                    "S3_BUCKET",
                    "S3_ACCESS_KEY_ID",
                    "S3_SECRET_ACCESS_KEY",
                ],
            )?;
            let region = var("S3_REGION").unwrap_or_else(|| "us-east-1".to_string());
            Box::new(S3Provider::new(
                &endpoint, bucket, access_key, secret_key, region,
            )?)
        }
    })
}

pub fn print_providers() {
    println!(
        "\
Supported cloud providers (-p, --provider):

  yandex   Yandex Disk (REST API)
           YANDEX_TOKEN                OAuth token with Disk REST API access

  mailru   Mail.ru Cloud (WebDAV)
           MAILRU_USER, MAILRU_PASSWORD  e-mail and an app password
           MAILRU_WEBDAV_URL           optional, default {MAILRU_WEBDAV_URL}

  google   Google Drive (API v3)
           GOOGLE_CLIENT_ID, GOOGLE_CLIENT_SECRET, GOOGLE_REFRESH_TOKEN
                                       recommended: tokens are renewed automatically
           GOOGLE_DRIVE_TOKEN          alternative: access token, valid ~1 hour

  webdav   Any WebDAV server (Nextcloud, ownCloud, Synology, ...)
           WEBDAV_URL, WEBDAV_USER, WEBDAV_PASSWORD
           WEBDAV_ETAG_IS_MD5          optional, 'true' if the server's ETag is the file MD5

  s3       S3 compatible (AWS, Cloudflare R2, MinIO, Yandex Object Storage)
           S3_ENDPOINT, S3_BUCKET, S3_ACCESS_KEY_ID, S3_SECRET_ACCESS_KEY
           S3_REGION                   optional, default us-east-1 (R2: auto)

Optional for URL mirroring: GITHUB_TOKEN (raises the GitHub API rate limit).

Variables are read from the environment and from a .env file (current folder,
next to the executable, or --env-file). Real environment variables take precedence."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> impl Iterator<Item = OsString> {
        list.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn finds_env_file_argument() {
        assert_eq!(
            find_env_file_arg(args(&["-p", "s3", "--env-file", "a.env", "x"])),
            Some(PathBuf::from("a.env"))
        );
        assert_eq!(
            find_env_file_arg(args(&["--env-file=b.env"])),
            Some(PathBuf::from("b.env"))
        );
        assert_eq!(find_env_file_arg(args(&["--", "--env-file", "c"])), None);
        assert_eq!(find_env_file_arg(args(&["dir"])), None);
    }
}
