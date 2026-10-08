pub mod clean;
pub mod local_upload;
pub mod remote_urls;

/// Counters reported at the end of a run; `failed > 0` yields a non-zero exit code.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub processed: u64,
    pub uploaded: u64,
    pub skipped: u64,
    pub deleted: u64,
    pub failed: u64,
    pub bytes_uploaded: u64,
}

impl Stats {
    pub fn merge(&mut self, other: Stats) {
        self.processed += other.processed;
        self.uploaded += other.uploaded;
        self.skipped += other.skipped;
        self.deleted += other.deleted;
        self.failed += other.failed;
        self.bytes_uploaded += other.bytes_uploaded;
    }
}
