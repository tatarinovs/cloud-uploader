mod client;
mod config;
mod modes;
mod storage;
mod utils;

use anyhow::{Context, Result};
use clap::Parser;
use config::ProviderKind;
use modes::local_upload::UploadOptions;
use modes::remote_urls::{MirrorOptions, UrlList, DEFAULT_ASSET_FILTER};
use modes::Stats;
use regex::Regex;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

const EXIT_FATAL: u8 = 1;
const EXIT_PARTIAL: u8 = 3;
const EXIT_INTERRUPTED: u8 = 130;

#[derive(Parser, Debug)]
#[command(
    name = "cloud-uploader",
    version,
    about = "Uploads backups to cloud storage and mirrors GitHub releases (Yandex Disk, Mail.ru, Google Drive, WebDAV, S3)",
    long_about = "Uploads a local file or directory to cloud storage, skipping files that are already \
up to date. Without LOCAL_PATH, mirrors the URLs listed in urls.txt (GitHub repositories are resolved \
to their latest release assets).\n\nCredentials are never passed on the command line: put them into a \
.env file or environment variables (see --list-providers).",
    after_help = "Exit codes: 0 success, 1 fatal error, 2 invalid arguments, 3 finished with failed files, 130 interrupted."
)]
struct Cli {
    /// Local file or directory to upload. If omitted, mirrors URLs from urls.txt
    #[arg(value_name = "LOCAL_PATH")]
    local_path: Option<PathBuf>,

    /// Cloud provider
    #[arg(
        short,
        long,
        env = "CLOUD_PROVIDER",
        value_enum,
        ignore_case = true,
        default_value = "yandex"
    )]
    provider: ProviderKind,

    /// Target folder in the cloud
    #[arg(short, long, env = "CLOUD_REMOTE_DIR", default_value = "/Upload")]
    remote: String,

    /// Path to a .env file (default: ./.env, then .env next to the executable)
    #[arg(long, value_name = "FILE")]
    env_file: Option<PathBuf>,

    /// URL list: a local file or a cloud path [default: ./urls.txt, <exe dir>/urls.txt, <remote>/urls.txt, /urls.txt]
    #[arg(long, value_name = "PATH", env = "CLOUD_URLS_FILE")]
    urls_file: Option<PathBuf>,

    /// Delete everything in the target folder before uploading (keeps a cloud urls.txt)
    #[arg(short = 'c', long = "clean", alias = "clean-remote")]
    clean: bool,

    /// Upload even if the remote file looks identical
    #[arg(short = 'f', long)]
    overwrite: bool,

    /// Show what would be uploaded or deleted without changing anything
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Number of files uploaded in parallel (local upload mode)
    #[arg(short, long, env = "CLOUD_JOBS", default_value_t = 2,
          value_parser = clap::value_parser!(u16).range(1..=32))]
    jobs: u16,

    /// Regex selecting which GitHub release assets to mirror
    #[arg(long, value_name = "REGEX", env = "CLOUD_ASSET_FILTER", default_value = DEFAULT_ASSET_FILTER)]
    assets: String,

    /// List supported providers and their configuration variables
    #[arg(long)]
    list_providers: bool,

    /// Verbose (debug) output
    #[arg(short, long, conflicts_with = "quiet")]
    verbose: bool,

    /// Only print warnings and errors
    #[arg(short, long)]
    quiet: bool,
}

enum Mode {
    Upload(PathBuf),
    Mirror(UrlList),
}

#[tokio::main]
async fn main() -> ExitCode {
    // Load .env before parsing so it can provide defaults (CLOUD_PROVIDER, ...).
    let env_loaded = config::load_environment(config::env_file_from_args().as_deref());
    let cli = Cli::parse();

    if cli.list_providers {
        config::print_providers();
        return ExitCode::SUCCESS;
    }
    init_logging(cli.verbose, cli.quiet);

    match env_loaded {
        Ok(Some(path)) => debug!("Loaded environment from {}", path.display()),
        Ok(None) => debug!("No .env file found, using environment variables only"),
        Err(err) => {
            error!("{err:#}");
            return ExitCode::from(EXIT_FATAL);
        }
    }

    let result = tokio::select! {
        result = run(&cli) => result,
        _ = tokio::signal::ctrl_c() => {
            warn!("Interrupted");
            return ExitCode::from(EXIT_INTERRUPTED);
        }
    };

    match result {
        Ok(stats) if stats.failed == 0 => {
            info!(
                "Done{}",
                if cli.dry_run {
                    " (dry run, nothing was changed)"
                } else {
                    ""
                }
            );
            ExitCode::SUCCESS
        }
        Ok(stats) => {
            error!("Finished with {} failure(s)", stats.failed);
            ExitCode::from(EXIT_PARTIAL)
        }
        Err(err) => {
            error!("{err:#}");
            ExitCode::from(EXIT_FATAL)
        }
    }
}

async fn run(cli: &Cli) -> Result<Stats> {
    let remote_dir = utils::path::normalize_remote_dir(&cli.remote);
    let asset_filter = Regex::new(&cli.assets).context("Invalid --assets regular expression")?;
    let provider = config::init_provider(cli.provider)?;

    info!(
        "cloud-uploader v{} | {} | target folder '{remote_dir}'{}",
        env!("CARGO_PKG_VERSION"),
        provider.name(),
        if cli.dry_run { " | DRY RUN" } else { "" }
    );

    // Everything that can fail is validated before anything is deleted.
    let mode = match &cli.local_path {
        Some(path) => {
            std::fs::metadata(path)
                .with_context(|| format!("Local path '{}' is not accessible", path.display()))?;
            Mode::Upload(path.clone())
        }
        None => Mode::Mirror(
            modes::remote_urls::load_url_list(&*provider, &remote_dir, cli.urls_file.as_deref())
                .await?,
        ),
    };

    let access = if cli.dry_run {
        provider.list_dir(&remote_dir).await.map(drop)
    } else {
        provider.check_access(&remote_dir).await
    };
    access.with_context(|| {
        format!(
            "Cannot access '{remote_dir}' on {}; check credentials and network",
            provider.name()
        )
    })?;

    let mut stats = Stats::default();
    if cli.clean {
        let keep: Vec<String> = match &mode {
            Mode::Mirror(list) => list.remote_path().map(str::to_string).into_iter().collect(),
            Mode::Upload(_) => Vec::new(),
        };
        stats.merge(
            modes::clean::clean_remote_dir(&*provider, &remote_dir, &keep, cli.dry_run).await?,
        );
    }

    match mode {
        Mode::Upload(path) => {
            let opts = UploadOptions {
                overwrite: cli.overwrite || cli.clean,
                dry_run: cli.dry_run,
                jobs: usize::from(cli.jobs),
            };
            stats.merge(
                modes::local_upload::run_local_upload(&*provider, &path, &remote_dir, &opts)
                    .await?,
            );
        }
        Mode::Mirror(list) => {
            let opts = MirrorOptions {
                dry_run: cli.dry_run,
                asset_filter,
            };
            stats.merge(
                modes::remote_urls::run_remote_urls(&*provider, &remote_dir, &list, &opts).await?,
            );
        }
    }
    Ok(stats)
}

fn init_logging(verbose: bool, quiet: bool) {
    let level = match (verbose, quiet) {
        (true, _) => "debug",
        (_, true) => "warn",
        _ => "info",
    };
    // CLOUD_UPLOADER_LOG accepts tracing filter syntax, e.g. "debug,reqwest=trace".
    let filter = EnvFilter::try_from_env("CLOUD_UPLOADER_LOG")
        .unwrap_or_else(|_| EnvFilter::new(format!("warn,cloud_uploader={level}")));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_timer(LocalTimer)
        .with_ansi(use_ansi())
        .init();
}

struct LocalTimer;

impl tracing_subscriber::fmt::time::FormatTime for LocalTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"))
    }
}

/// Colors only for interactive terminals, never in log files; honors NO_COLOR.
fn use_ansi() -> bool {
    std::env::var_os("NO_COLOR").is_none()
        && std::io::stdout().is_terminal()
        && enable_virtual_terminal()
}

#[cfg(windows)]
fn enable_virtual_terminal() -> bool {
    use std::os::raw::c_void;
    type Handle = *mut c_void;

    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    extern "system" {
        fn GetStdHandle(std_handle: u32) -> Handle;
        fn GetConsoleMode(console: Handle, mode: *mut u32) -> i32;
        fn SetConsoleMode(console: Handle, mode: u32) -> i32;
    }

    // SAFETY: plain Win32 console calls on this process's own stdout handle.
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() || handle == (-1isize as Handle) {
            return false;
        }
        let mut mode = 0u32;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return false;
        }
        mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
            || SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

#[cfg(not(windows))]
fn enable_virtual_terminal() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn provider_aliases_are_accepted() {
        let cli = Cli::try_parse_from(["x", "-p", "GDrive", "-j", "4", "dir"]).unwrap();
        assert_eq!(cli.provider, ProviderKind::Google);
        assert_eq!(cli.jobs, 4);
        assert!(Cli::try_parse_from(["x", "-j", "0"]).is_err());
    }
}
