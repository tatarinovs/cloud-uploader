use anyhow::{Context, Result};
use md5::{Digest, Md5};
use std::path::Path;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

pub async fn compute_md5_file<P: AsRef<Path>>(path: P) -> Result<String> {
    let path_ref = path.as_ref();
    let mut file = File::open(path_ref).await.with_context(|| {
        format!(
            "Failed to open file for MD5 computation: {}",
            path_ref.display()
        )
    })?;

    let mut hasher = Md5::new();
    let mut buffer = [0u8; 65536];

    loop {
        let n = file
            .read(&mut buffer)
            .await
            .with_context(|| format!("Failed to read chunk from file: {}", path_ref.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }

    Ok(hex::encode(hasher.finalize()))
}
