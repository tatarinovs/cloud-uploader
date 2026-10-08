use anyhow::{Context, Result};
use md5::{Digest, Md5};
use std::path::Path;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

/// Streams a file through MD5 and returns the lowercase hex digest.
pub async fn compute_md5_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .await
        .with_context(|| format!("Failed to open '{}' for hashing", path.display()))?;

    let mut hasher = Md5::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = file
            .read(&mut buffer)
            .await
            .with_context(|| format!("Failed to read '{}'", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }

    Ok(hex::encode(hasher.finalize()))
}
