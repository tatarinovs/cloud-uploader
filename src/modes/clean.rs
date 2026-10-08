use super::Stats;
use crate::storage::StorageProvider;
use crate::utils::path::normalize_remote_dir;
use anyhow::{bail, Context, Result};
use tracing::{error, info};

/// Deletes everything inside `remote_dir` except the paths listed in `keep`.
pub async fn clean_remote_dir(
    provider: &dyn StorageProvider,
    remote_dir: &str,
    keep: &[String],
    dry_run: bool,
) -> Result<Stats> {
    let dir = normalize_remote_dir(remote_dir);
    if dir == "/" {
        bail!("Refusing to clean the root folder '/'. Specify a subfolder, e.g. -r /Upload");
    }

    info!("Cleaning remote folder '{dir}'...");
    let items = provider
        .list_dir(&dir)
        .await
        .with_context(|| format!("Failed to list '{dir}' for cleaning"))?;

    let mut stats = Stats::default();
    for item in items {
        if keep.contains(&item.path) {
            info!("  [=] Keeping {}", item.path);
            continue;
        }
        if dry_run {
            info!("  [dry-run] Would delete {}", item.path);
            stats.deleted += 1;
            continue;
        }
        match provider.delete(&item.path, item.is_dir).await {
            Ok(()) => {
                info!("  [-] Deleted {}", item.path);
                stats.deleted += 1;
            }
            Err(err) => {
                error!("  [!] Failed to delete '{}': {err:#}", item.path);
                stats.failed += 1;
            }
        }
    }
    info!("Cleaned {} item(s) from '{dir}'", stats.deleted);
    Ok(stats)
}
