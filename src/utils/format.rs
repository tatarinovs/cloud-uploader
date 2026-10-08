use std::time::Duration;

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut b = bytes as f64;
    let mut i = 0;
    while b >= 1024.0 && i < UNITS.len() - 1 {
        b /= 1024.0;
        i += 1;
    }
    format!("{:.1} {}", b, UNITS[i])
}

/// Average transfer rate, e.g. "12.3 MB/s".
pub fn format_rate(bytes: u64, elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64().max(0.001);
    format!("{}/s", format_bytes((bytes as f64 / secs) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1048576), "1.0 MB");
        assert_eq!(format_bytes(1073741824), "1.0 GB");
    }

    #[test]
    fn test_format_rate() {
        assert_eq!(format_rate(2048, Duration::from_secs(2)), "1.0 KB/s");
    }
}
