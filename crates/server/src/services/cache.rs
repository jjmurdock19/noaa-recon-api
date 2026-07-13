//! Disk-based result cache with a lock-file + TTL pattern — port of
//! `app/services/cache.py`. Backs the GOES tile pipeline: a `<key>.lock` marks
//! "rendering in progress", a `<key>.json` holds the final result.

use std::path::PathBuf;

use serde_json::{Map, Value};

pub struct ResultCache {
    base_dir: PathBuf,
    lock_timeout: u64,
}

impl ResultCache {
    pub fn new(base_dir: PathBuf, lock_timeout: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(&base_dir)?;
        Ok(Self { base_dir, lock_timeout })
    }

    fn paths(&self, key: &str) -> (PathBuf, PathBuf) {
        (
            self.base_dir.join(format!("{key}.json")),
            self.base_dir.join(format!("{key}.lock")),
        )
    }

    /// Port of `get_status`. Returns the ready/error result, a synthesized
    /// "generating" object (with elapsed seconds) while a fresh lock is held, or
    /// `None` if unknown / the lock went stale (which it also clears).
    pub fn get_status(&self, key: &str) -> Option<Value> {
        let (json_path, lock_path) = self.paths(key);
        if let Ok(text) = std::fs::read_to_string(&json_path) {
            if let Ok(meta) = serde_json::from_str::<Value>(&text) {
                if matches!(meta.get("status").and_then(Value::as_str), Some("ready" | "error")) {
                    return Some(meta);
                }
            }
        }
        if let Ok(md) = std::fs::metadata(&lock_path) {
            let age = md
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if age > self.lock_timeout {
                let _ = std::fs::remove_file(&lock_path);
                return None;
            }
            // Start from whatever params acquire_lock persisted (band/cmap/etc.),
            // then overlay status/key/elapsed.
            let mut obj: Map<String, Value> = std::fs::read_to_string(&lock_path)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            obj.insert("status".into(), Value::from("generating"));
            obj.insert("key".into(), Value::from(key));
            obj.insert("elapsed".into(), Value::from(age));
            return Some(Value::Object(obj));
        }
        None
    }

    /// Port of `acquire_lock`: drop any prior result, write the params into the
    /// lock file so `get_status` can report them while still generating.
    pub fn acquire_lock(&self, key: &str, params: Option<&Value>) -> std::io::Result<()> {
        let (json_path, lock_path) = self.paths(key);
        let _ = std::fs::remove_file(&json_path);
        let body = params.cloned().unwrap_or_else(|| Value::Object(Map::new()));
        std::fs::write(&lock_path, serde_json::to_vec(&body).unwrap_or_default())
    }

    pub fn write_result(&self, key: &str, meta: &Value) -> std::io::Result<()> {
        let (json_path, lock_path) = self.paths(key);
        std::fs::write(&json_path, serde_json::to_vec(meta).unwrap_or_default())?;
        let _ = std::fs::remove_file(&lock_path);
        Ok(())
    }

    pub fn output_path(&self, key: &str, suffix: &str) -> PathBuf {
        self.base_dir.join(format!("{key}.{suffix}"))
    }

    /// All keys with a `.json` or `.lock` file.
    pub fn list_keys(&self) -> Vec<String> {
        let mut keys = std::collections::BTreeSet::new();
        if let Ok(entries) = std::fs::read_dir(&self.base_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if matches!(path.extension().and_then(|e| e.to_str()), Some("json" | "lock")) {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        keys.insert(stem.to_string());
                    }
                }
            }
        }
        keys.into_iter().collect()
    }

    /// Remove every file for `key`; returns bytes freed.
    pub fn delete(&self, key: &str) -> u64 {
        let mut freed = 0u64;
        let prefix = format!("{key}.");
        if let Ok(entries) = std::fs::read_dir(&self.base_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with(&prefix) {
                    freed += entry.metadata().map(|m| m.len()).unwrap_or(0);
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        freed
    }

    pub fn stats(&self) -> Value {
        let (mut count, mut total) = (0u64, 0u64);
        if let Ok(entries) = std::fs::read_dir(&self.base_dir) {
            for entry in entries.flatten() {
                if entry.metadata().map(|m| m.is_file()).unwrap_or(false) {
                    count += 1;
                    total += entry.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
        serde_json::json!({ "file_count": count, "bytes": total })
    }
}
