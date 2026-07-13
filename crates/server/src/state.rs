//! Shared application state.
//!
//! FastAPI leaned on module-level globals and per-request DB connections. In
//! axum the idiomatic equivalent is a single `AppState` cloned into every
//! handler (cheap — everything inside is `Arc`). DB connections are still opened
//! per-request (mirroring Python's `get_connection()`); the paths to open them
//! live here. `stats` is the process-wide request counter (was a Python global).

use std::sync::Arc;

use axum::extract::FromRef;
use axum_extra::extract::cookie::Key;

use crate::config::Paths;
use crate::services::stats::Stats;

#[derive(Clone)]
pub struct AppState {
    pub paths: Arc<Paths>,
    pub stats: Arc<Stats>,
    /// Signing key for the admin session cookie (derived from admin_credentials.json).
    pub cookie_key: Key,
}

impl AppState {
    pub fn new(paths: Paths, cookie_key: Key) -> Self {
        Self {
            paths: Arc::new(paths),
            stats: Arc::new(Stats::default()),
            cookie_key,
        }
    }
}

/// Lets axum-extra's `SignedCookieJar` pull the signing key out of `AppState`.
impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Self {
        state.cookie_key.clone()
    }
}
