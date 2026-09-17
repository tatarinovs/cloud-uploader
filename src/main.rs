mod client;
mod config;
mod modes;
mod storage;
mod utils;

use anyhow::{bail, Result};
use clap::Parser;
use std::path::PathBuf;
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

#[derive(Parser, Debug)]
#[command(
    name = "cloud-uploader",
    author = "Cloud Uploader Team",
    version,
    about = "Universal cloud backup and release mirror utility (Yandex, Mail.ru, Google Drive, WebDAV, S3)",
    long_about = "Securely uploads local files/folders to cloud storage or synchronizes GitHub releases from a remote urls.txt list.\n\nCredentials are NEVER passed via CLI arguments; configure them in a .env file or system environment variables for security."
)]
struct Cli {
    /// Local file or directory to upload. If omitted, runs remote URLs mirror mode using urls.txt from cloud
    #[arg(value_name = "LOCAL_PATH")]
    local_path: Option<PathBuf>,

    /// Cloud provider: yandex, mailru, google, webdav, s3
    #[arg(
        short = 'p',
        long = "provider",
        env = "CLOUD_PROVIDER",
        default_value = "yandex",
        value_parser = ["yandex", "mailru", "google", "webdav", "s3"]
    )]
    provider: String,

    /// Show list of supported cloud providers and their required .env variables
    #[arg(long = "list-providers", default_value_t = false)]
    list_providers: bool,

    /// Remote target directory in cloud (e.g. /Upload or /Backups/2026.09.17)
    #[arg(
        short = 'r',
        long = "remote",
        env = "CLOUD_REMOTE_DIR",
        default_value = "/Upload"
    )]
    remote: String,

    /// Custom path to .env file
    #[arg(long = "env-file", value_name = "FILE")]
    env_file: Option<PathBuf>,

    /// Custom path to urls.txt (local file or cloud path). If omitted, checks local urls.txt before cloud
    #[arg(long = "urls-file", value_name = "PATH")]
    urls_file: Option<PathBuf>,

    /// Clean (empty) target remote folder in the cloud before uploading
    #[arg(
        short = 'c',
        long = "clean",
        alias = "clean-remote",
        default_value_t = false
    )]
    clean_remote: bool,

    /// Force overwrite existing remote files even if size or MD5 match
    #[arg(short = 'f', long = "overwrite", default_value_t = false)]
    overwrite: bool,

    /// Enable verbose / debug logging
    #[arg(short = 'v', long = "verbose", default_value_t = false)]
    verbose: bool,
}

#[cfg(windows)]
#[allow(clippy::upper_case_acronyms)]
fn init_ansi_support() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }

    unsafe {
        use std::os::raw::c_void;
        type HANDLE = *mut c_void;
        type DWORD = u32;
        type BOOL = i32;

        const STD_OUTPUT_HANDLE: DWORD = -11i32 as DWORD;
        const ENABLE_VIRTUAL_TERMINAL_PROCESSING: DWORD = 0x0004;

        extern "system" {
            fn GetStdHandle(nStdHandle: DWORD) -> HANDLE;
            fn GetConsoleMode(hConsoleHandle: HANDLE, lpMode: *mut DWORD) -> BOOL;
            fn SetConsoleMode(hConsoleHandle: HANDLE, dwMode: DWORD) -> BOOL;
        }

        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() || handle == (-1isize as *mut c_void) {
            return false;
        }

        let mut mode: DWORD = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return false;
        }

        if (mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0 {
            return true;
        }

        mode |= ENABLE_VIRTUAL_TERMINAL_PROCESSING;
        SetConsoleMode(handle, mode) != 0
    }
}

#[cfg(not(windows))]
fn init_ansi_support() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

struct CompactLocalTimer;

impl tracing_subscriber::fmt::time::FormatTime for CompactLocalTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.list_providers {
        println!("Supported cloud providers (-p, --provider):");
        println!("  - yandex   : Yandex.Disk (REST API OAuth)");
        println!("               Required env: YANDEX_TOKEN");
        println!();
        println!("  - mailru   : Mail.ru Cloud (WebDAV: webdav.mail.ru)");
        println!("               Required env: MAILRU_USER, MAILRU_PASSWORD");
        println!();
        println!("  - google   : Google Drive (v3 API)");
        println!("               Required env: GOOGLE_DRIVE_TOKEN");
        println!();
        println!("  - webdav   : Universal WebDAV (Nextcloud, ownCloud, pCloud, NAS)");
        println!("               Required env: WEBDAV_URL, WEBDAV_USER, WEBDAV_PASSWORD");
        println!();
        println!("  - s3       : S3 Compatible / Cloudflare R2 / MinIO / Yandex Object Storage");
        println!("               Required env: S3_ENDPOINT, S3_BUCKET, S3_ACCESS_KEY_ID, S3_SECRET_ACCESS_KEY (optional: S3_REGION)");
        println!();
        println!("Note: All credentials can be placed in .env file next to the binary or current folder.");
        return Ok(());
    }

    // 1. Setup logging
    FmtSubscriber::builder()
        .with_max_level(if cli.verbose {
            Level::DEBUG
        } else {
            Level::INFO
        })
        .with_target(false)
        .with_timer(CompactLocalTimer)
        .with_ansi(init_ansi_support())
        .init();

    info!("=== Cloud Uploader v{} ===", env!("CARGO_PKG_VERSION"));

    // 2. Load .env environment
    if let Err(e) = config::load_environment(cli.env_file.as_deref()) {
        bail!("Environment loading error: {}", e);
    }

    // Clean remote path
    let remote_dir = if cli.remote.trim().is_empty() || cli.remote.trim() == "/" {
        "/Upload".to_string()
    } else {
        format!("/{}", cli.remote.trim().trim_matches('/'))
    };

    // 3. Initialize cloud provider
    let provider = match config::init_provider(&cli.provider) {
        Ok(p) => p,
        Err(e) => {
            error!("ERROR: {}", e);
            std::process::exit(1);
        }
    };

    info!("Selected provider:   {}", provider.name());
    info!("Target cloud folder: {}", remote_dir);

    // 4. Ensure target folder exists - FAIL FAST if unauthorized or invalid
    if let Err(err) = provider.ensure_dir(&remote_dir).await {
        bail!(
            "Failed to access or create target remote directory '{}': {}\nPlease verify your credentials and network connectivity.",
            remote_dir,
            err
        );
    }

    // Optional: Clean remote target folder if requested
    if cli.clean_remote {
        clean_remote_dir(&*provider, &remote_dir).await?;
    }

    // 5. Dispatch mode
    if let Some(ref local_path) = cli.local_path {
        // Mode 1: Local upload mode
        modes::local_upload::run_local_upload(&*provider, local_path, &remote_dir, cli.overwrite)
            .await?;
    } else {
        // Mode 2: Remote URLs mirror mode
        modes::remote_urls::run_remote_urls(&*provider, &remote_dir, cli.urls_file.as_deref())
            .await?;
    }

    Ok(())
}

async fn clean_remote_dir(provider: &dyn storage::StorageProvider, remote_dir: &str) -> Result<()> {
    let clean_path = remote_dir.trim().trim_end_matches('/');
    if clean_path.is_empty() || clean_path == "/" {
        bail!("Refusing to clean root directory '/' for safety! Specify a target subfolder (e.g. -r /Upload).");
    }

    info!(
        "Cleaning target remote directory '{}' before operation...",
        clean_path
    );
    let items = match provider.list_dir(clean_path).await {
        Ok(i) => i,
        Err(e) => {
            warn!(
                "Failed to list items for cleaning in '{}': {}",
                clean_path, e
            );
            return Ok(());
        }
    };

    if items.is_empty() {
        info!("Remote directory '{}' is already empty.", clean_path);
        return Ok(());
    }

    let mut deleted_count = 0;
    for item in items {
        info!("  [-] Deleting remote: {}", item.path);
        if let Err(e) = provider.delete_file(&item.path).await {
            warn!("  [!] Failed to delete '{}': {}", item.path, e);
        } else {
            deleted_count += 1;
        }
    }

    info!(
        "[OK] Cleaned {} item(s) from '{}'.",
        deleted_count, clean_path
    );
    Ok(())
}
