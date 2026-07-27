//! Shared SQLite connection setup.
//!
//! Every `get_connection`/`init_db` across the service modules opens through
//! here instead of a bare `Connection::open`. Two settings matter for a
//! multi-connection, per-request-open server like this one:
//!
//! - `journal_mode = WAL`: readers no longer block behind a writer (e.g. an
//!   `archive_update`/`tdr_ingest` job). This is stored in the database file
//!   itself, so it only needs to be set once per file, but setting it on
//!   every open is idempotent and cheap.
//! - `busy_timeout`: the rare case where two writers do collide (e.g. ingest
//!   plus a concurrent admin write) retries for a few seconds instead of
//!   failing immediately with `SQLITE_BUSY`.
//! - `synchronous = NORMAL`: the standard companion to WAL. Every commit
//!   otherwise fsyncs the WAL, which dominates the runtime of a bulk ingest.
//!   NORMAL cannot corrupt the database — the only exposure is that a commit
//!   made in the seconds before an OS crash or power loss may be lost, and
//!   every database here is a re-derivable archive cache that a re-run of the
//!   corresponding ingest rebuilds.
use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub fn open(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}
