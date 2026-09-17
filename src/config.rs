use crate::storage::google::GoogleDriveProvider;
use crate::storage::mailru::MailRuProvider;
use crate::storage::s3::S3Provider;
use crate::storage::webdav::WebDavProvider;
use crate::storage::yandex::YandexDiskProvider;
use crate::storage::StorageProvider;
use anyhow::{bail, Context, Result};
use std::env;
use std::path::Path;
use tracing::{debug, info};

pub fn load_environment(explicit_env_file: Option<&Path>) -> Result<()> {
    if let Some(path) = explicit_env_file {
        if path.exists() {
            dotenvy::from_path(path).with_context(|| {
                format!(
                    "Failed to load specified .env file from: {}",
                    path.display()
                )
            })?;
            info!("Loaded environment from custom file: {}", path.display());
            return Ok(());
        } else {
            bail!("Specified .env file does not exist: {}", path.display());
        }
    }

    // 1. Try standard current working directory
    if let Ok(env_path) = dotenvy::dotenv() {
        debug!("Loaded .env from: {}", env_path.display());
        return Ok(());
    }

    // 2. Try adjacent to executable
    if let Ok(exe_path) = env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let candidate = exe_dir.join(".env");
            if candidate.exists() && dotenvy::from_path(&candidate).is_ok() {
                debug!(
                    "Loaded .env from executable directory: {}",
                    candidate.display()
                );
                return Ok(());
            }
        }
    }

    debug!("No .env file found; using existing system environment variables");
    Ok(())
}

pub fn init_provider(provider_name: &str) -> Result<Box<dyn StorageProvider>> {
    match provider_name.trim().to_lowercase().as_str() {
        "yandex" | "ya" | "yadisk" | "yandex.ru" | "ya.ru" => {
            let token = env::var("YANDEX_TOKEN").unwrap_or_default();
            if token.is_empty() {
                bail!(
                    "Yandex OAuth token is missing!\nSet YANDEX_TOKEN in your .env file or environment variables."
                );
            }
            Ok(Box::new(YandexDiskProvider::new(token)?))
        }

        "mailru" | "mail" | "cloudmail" | "mail.ru" | "cloud.mail.ru" => {
            let user = env::var("MAILRU_USER").unwrap_or_default();
            let password = env::var("MAILRU_PASSWORD").unwrap_or_default();
            if user.is_empty() || password.is_empty() {
                bail!(
                    "Mail.ru credentials are missing!\nSet MAILRU_USER and MAILRU_PASSWORD in your .env file or environment variables."
                );
            }
            Ok(Box::new(MailRuProvider::new(user, password)?))
        }

        "google" | "gdrive" | "googledrive" | "drive" => {
            let token = env::var("GOOGLE_DRIVE_TOKEN").unwrap_or_default();
            if token.is_empty() {
                bail!(
                    "Google Drive OAuth token is missing!\nSet GOOGLE_DRIVE_TOKEN in your .env file or environment variables."
                );
            }
            Ok(Box::new(GoogleDriveProvider::new(token)?))
        }

        "webdav" | "nextcloud" | "owncloud" | "pcloud" => {
            let url = env::var("WEBDAV_URL").unwrap_or_default();
            let user = env::var("WEBDAV_USER").unwrap_or_default();
            let password = env::var("WEBDAV_PASSWORD").unwrap_or_default();
            if url.is_empty() || user.is_empty() || password.is_empty() {
                bail!(
                    "WebDAV configuration is missing!\nSet WEBDAV_URL, WEBDAV_USER and WEBDAV_PASSWORD in your .env file or environment variables."
                );
            }
            Ok(Box::new(WebDavProvider::new(
                "Universal WebDAV",
                url,
                user,
                password,
            )?))
        }

        "s3" | "r2" | "cloudflare" | "minio" => {
            let endpoint = env::var("S3_ENDPOINT").unwrap_or_default();
            let bucket = env::var("S3_BUCKET").unwrap_or_default();
            let access_key = env::var("S3_ACCESS_KEY_ID").unwrap_or_default();
            let secret_key = env::var("S3_SECRET_ACCESS_KEY").unwrap_or_default();
            let region = env::var("S3_REGION").ok();

            if endpoint.is_empty()
                || bucket.is_empty()
                || access_key.is_empty()
                || secret_key.is_empty()
            {
                bail!(
                    "S3 configuration is missing!\nSet S3_ENDPOINT, S3_BUCKET, S3_ACCESS_KEY_ID, and S3_SECRET_ACCESS_KEY in your .env file."
                );
            }

            Ok(Box::new(S3Provider::new(
                endpoint, bucket, access_key, secret_key, region,
            )?))
        }

        other => {
            bail!(
                "Unsupported cloud provider '{}'. Supported providers: 'yandex', 'mailru', 'google', 'webdav', 's3'",
                other
            );
        }
    }
}
