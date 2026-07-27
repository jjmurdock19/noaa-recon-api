//! Live "what is this job doing right now" reporting for long-running ingests.
//!
//! An archive update is minutes of HTTP fetches and SQLite writes behind a
//! single `status: "running"`, which gives an operator no way to tell a job
//! that's working from one that's wedged — the console just polls forever.
//! A [`Progress`] handle threaded into each ingest fixes that: the ingest
//! publishes a one-line phase plus a step counter as it goes, and whoever is
//! polling reads the latest snapshot.
//!
//! Deliberately a shared cell rather than a channel: the reader only ever
//! wants the *current* state, never the history, so there's no queue to drain
//! and no backpressure to think about. Every method is non-blocking and
//! infallible — progress reporting must never be able to fail an ingest.
//!
//! CLI ingests construct a `Progress::default()` and drop it; nothing reads
//! the reports and the cost is one uncontended mutex per update.

use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde::Serialize;

/// A snapshot of where a job has got to. `None`/empty fields mean "not known
/// yet" rather than zero, so the console can leave the line blank instead of
/// rendering a misleading `0/0`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    /// The current phase, e.g. `"HURDAT2 AL"` or `"2026 recon missions"`.
    pub phase: String,
    /// Optional finer-grained detail within the phase, typically the item
    /// being worked on right now (a mission id, a filename).
    pub detail: String,
    /// Items finished within the current phase.
    pub done: i64,
    /// Items in the current phase, when that's known before starting it.
    /// Crawled listings usually do know; open-ended work doesn't.
    pub total: Option<i64>,
    /// RFC3339 timestamp of the last update. This is the field that actually
    /// answers "is it hung?" — a stale `updated_at` means no forward motion,
    /// which the counter alone can't distinguish from a slow single item.
    pub updated_at: Option<String>,
}

/// Cloneable handle to one job's [`Report`]. Clones share the same cell.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    inner: Arc<Mutex<Report>>,
}

impl Progress {
    /// Starts a new phase, resetting the counter and clearing any detail.
    /// `total` is the item count for the phase when it's known up front.
    pub fn phase(&self, phase: impl Into<String>, total: Option<i64>) {
        self.update(|r| {
            r.phase = phase.into();
            r.detail = String::new();
            r.done = 0;
            r.total = total;
        });
    }

    /// Attaches an item count to the phase already in flight, without
    /// disturbing the counter. For work whose size only becomes known partway
    /// in, once a listing or a source file has been fetched.
    pub fn set_total(&self, total: i64) {
        self.update(|r| r.total = Some(total));
    }

    /// Records the item about to be worked on, without advancing the counter.
    /// Call this before a slow step so the console names what it's waiting on.
    pub fn detail(&self, detail: impl Into<String>) {
        self.update(|r| r.detail = detail.into());
    }

    /// Advances the counter by one, for an item that just finished.
    pub fn step(&self) {
        self.update(|r| r.done += 1);
    }

    pub fn snapshot(&self) -> Report {
        self.inner.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// Every mutation stamps `updated_at`, so no caller can report motion
    /// without also refreshing the liveness clock the console reads.
    ///
    /// A poisoned mutex is ignored rather than propagated: the lock is only
    /// ever held for these few field writes, so poisoning means an unrelated
    /// panic, and losing a progress line is not a reason to fail an ingest.
    fn update(&self, f: impl FnOnce(&mut Report)) {
        if let Ok(mut r) = self.inner.lock() {
            f(&mut r);
            r.updated_at = Some(Utc::now().to_rfc3339());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_empty_with_no_liveness_clock() {
        let r = Progress::default().snapshot();
        assert_eq!(r.phase, "");
        assert_eq!(r.done, 0);
        assert_eq!(r.total, None);
        // Nothing has happened yet, so there is no "last moved" to report —
        // the console must not read a fresh handle as a live job.
        assert_eq!(r.updated_at, None);
    }

    #[test]
    fn phase_resets_counter_and_detail() {
        let p = Progress::default();
        p.phase("HURDAT2 AL", Some(3));
        p.detail("KATRINA");
        p.step();
        p.step();
        assert_eq!(p.snapshot().done, 2);

        p.phase("ATCF AL 2026", None);
        let r = p.snapshot();
        assert_eq!(r.phase, "ATCF AL 2026");
        assert_eq!(r.detail, "");
        assert_eq!(r.done, 0);
        assert_eq!(r.total, None);
    }

    #[test]
    fn set_total_leaves_counter_alone() {
        let p = Progress::default();
        p.phase("Level 1b missions", None);
        p.step();
        p.set_total(47);
        let r = p.snapshot();
        assert_eq!(r.done, 1);
        assert_eq!(r.total, Some(47));
    }

    #[test]
    fn every_mutation_stamps_the_liveness_clock() {
        for mutate in [
            &(|p: &Progress| p.phase("x", None)) as &dyn Fn(&Progress),
            &|p: &Progress| p.set_total(1),
            &|p: &Progress| p.detail("y"),
            &|p: &Progress| p.step(),
        ] {
            let p = Progress::default();
            mutate(&p);
            assert!(p.snapshot().updated_at.is_some());
        }
    }

    #[test]
    fn clones_share_one_cell() {
        let p = Progress::default();
        let handed_to_the_ingest = p.clone();
        handed_to_the_ingest.phase("ATCF EP 2026", Some(9));
        handed_to_the_ingest.step();
        let seen_by_the_poller = p.snapshot();
        assert_eq!(seen_by_the_poller.phase, "ATCF EP 2026");
        assert_eq!(seen_by_the_poller.done, 1);
    }
}
