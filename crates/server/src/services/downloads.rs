//! Process-wide registry of in-flight raw netCDF downloads.
//!
//! Lets the console's "Raw netCDF cache" panel show live per-file download
//! progress (bytes/total, elapsed) no matter which query, composite, or
//! future prefetch job triggered the fetch — not just the one render job an
//! operator happens to have the "Submit a query" status poll open for (see
//! `ProgressTracker` in `services/goes.rs`, which is per-job).
//!
//! Ephemeral and in-memory only: an entry exists only while
//! `ensure_downloaded` is streaming that file to disk. Once it's renamed
//! into place (or the download fails) the entry is removed and the file
//! shows up in the normal `/v1/admin/cache/goes_nc` listing instead.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use serde_json::{json, Value};

#[derive(Default)]
pub struct DownloadsRegistry {
    inner: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    bytes: u64,
    total_bytes: u64,
    started_at: Instant,
}

impl DownloadsRegistry {
    pub fn start(&self, filename: &str, total_bytes: u64) {
        self.inner.lock().unwrap().insert(
            filename.to_string(),
            Entry { bytes: 0, total_bytes, started_at: Instant::now() },
        );
    }

    pub fn update(&self, filename: &str, bytes: u64) {
        if let Some(e) = self.inner.lock().unwrap().get_mut(filename) {
            e.bytes = bytes;
        }
    }

    pub fn finish(&self, filename: &str) {
        self.inner.lock().unwrap().remove(filename);
    }

    /// Snapshot for the console, most-recently-started first.
    pub fn snapshot(&self) -> Vec<Value> {
        let inner = self.inner.lock().unwrap();
        let mut entries: Vec<(&String, &Entry)> = inner.iter().collect();
        entries.sort_by_key(|(_, e)| std::cmp::Reverse(e.started_at));
        entries
            .into_iter()
            .map(|(name, e)| {
                json!({
                    "filename": name,
                    "bytes": e.bytes,
                    "total_bytes": e.total_bytes,
                    "elapsed_seconds": e.started_at.elapsed().as_secs(),
                })
            })
            .collect()
    }
}
