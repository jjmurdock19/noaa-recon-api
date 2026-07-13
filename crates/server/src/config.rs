//! Filesystem layout and runtime settings.
//!
//! Port of `app/paths.py`. Where the Python module computes `REPO_ROOT` from
//! `__file__`, a compiled binary has no `__file__`, so we resolve the repo root
//! from the current working directory (the systemd unit / CLI always launches
//! the server from the install dir) with an env-var override for flexibility.

use std::path::PathBuf;

/// Resolved, process-wide paths. Built once at startup and shared via `AppState`.
#[derive(Clone, Debug)]
pub struct Paths {
    pub repo_root: PathBuf,
    pub cache_root: PathBuf,
    pub data_root: PathBuf,
    pub storms_db: PathBuf,
    pub recon_met_db: PathBuf,
    pub auth_db: PathBuf,
}

impl Paths {
    /// Mirror of `paths.py`: derive everything from the repo root and create the
    /// `cache/` and `data/` directories eagerly, exactly as the Python module does
    /// at import time.
    pub fn resolve() -> std::io::Result<Self> {
        let repo_root = match std::env::var_os("NOAA_RECON_REPO_ROOT") {
            Some(v) => PathBuf::from(v),
            None => std::env::current_dir()?,
        };
        let cache_root = repo_root.join("cache");
        let data_root = repo_root.join("data");
        std::fs::create_dir_all(&cache_root)?;
        std::fs::create_dir_all(&data_root)?;

        Ok(Self {
            storms_db: data_root.join("storms.sqlite"),
            recon_met_db: data_root.join("recon_met.sqlite"),
            auth_db: data_root.join("auth.sqlite"),
            repo_root,
            cache_root,
            data_root,
        })
    }

    pub fn console_dir(&self) -> PathBuf {
        self.repo_root.join("app").join("console")
    }

    pub fn netcdf_three_demo_dir(&self) -> PathBuf {
        self.repo_root.join("clients").join("netcdf-three-demo")
    }

    pub fn llms_txt(&self) -> PathBuf {
        self.repo_root.join("llms.txt")
    }
}

/// Bind address for the server. `uvicorn` defaults to 127.0.0.1:8000; the
/// installer sets the real port via the systemd unit's `--port`. We read the
/// same `NOAA_RECON_HOST` / `PORT` knobs so the two versions are drop-in
/// interchangeable behind the same reverse proxy for benchmarking.
pub fn bind_addr() -> String {
    let host = std::env::var("NOAA_RECON_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("PORT").unwrap_or_else(|_| "8000".into());
    format!("{host}:{port}")
}
