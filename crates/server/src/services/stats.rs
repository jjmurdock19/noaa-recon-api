//! In-memory request counters — port of `app/services/stats.py`.
//!
//! Not persisted; resets on restart (a live "calls in the last hour" gauge, not
//! an audit log — logs/app.log covers that). Python used module globals; here
//! it's a struct held in `AppState` (shared via `Arc`), which is the idiomatic
//! axum equivalent and stays correct if we ever run multi-threaded.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;

const WINDOW_SECS: u64 = 3600;

pub struct Stats {
    start: Instant,
    total: AtomicU64,
    times: Mutex<VecDeque<Instant>>,
}

#[derive(Serialize)]
pub struct PublicStats {
    pub healthy: bool,
    pub uptime_seconds: u64,
    pub calls_last_hour: usize,
    pub total_calls: u64,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            total: AtomicU64::new(0),
            times: Mutex::new(VecDeque::new()),
        }
    }
}

impl Stats {
    fn prune(&self, times: &mut VecDeque<Instant>, now: Instant) {
        while let Some(&front) = times.front() {
            if now.duration_since(front).as_secs() > WINDOW_SECS {
                times.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn record_request(&self) {
        let now = Instant::now();
        let mut times = self.times.lock().unwrap();
        times.push_back(now);
        self.total.fetch_add(1, Ordering::Relaxed);
        self.prune(&mut times, now);
    }

    pub fn public(&self) -> PublicStats {
        let now = Instant::now();
        let mut times = self.times.lock().unwrap();
        self.prune(&mut times, now);
        PublicStats {
            healthy: true,
            uptime_seconds: now.duration_since(self.start).as_secs(),
            calls_last_hour: times.len(),
            total_calls: self.total.load(Ordering::Relaxed),
        }
    }
}
