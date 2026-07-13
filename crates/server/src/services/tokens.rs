//! API token / admin-account store — port of `app/services/tokens.py`.
//!
//! Backs both the optional public-API token gate (`auth::require_api_token`)
//! and the admin console's per-person login. Three roles, one `tokens` table:
//! `regular` (plain API key), `moderator` (console login, limited), `superuser`
//! (console login, unrestricted).
//!
//! Parity notes vs the Python version:
//!   * Same schema string, idempotent `CREATE ... IF NOT EXISTS`.
//!   * Token hashing = SHA-256 hex (fast; tokens are high-entropy). Password
//!     hashing = PBKDF2-HMAC-SHA256, 310_000 iters, 16-byte salt (slow KDF).
//!   * `secrets.token_urlsafe(32)` -> 32 random bytes, URL-safe base64, no pad.
//!   * Fresh connection per call, mirroring Python's `get_connection()` model.
//!     (A pool is a later optimization; noted for the benchmark.)

// The auth gate + admin router (which consume most of this) land next; until
// then these public fns have no non-test caller.
#![allow(dead_code)]

use std::path::Path;

use base64::Engine;
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, Row};
use serde::Serialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// OWASP's current PBKDF2-HMAC-SHA256 baseline (matches `PBKDF2_ITERATIONS`).
const PBKDF2_ITERATIONS: u32 = 310_000;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tokens (
    id            INTEGER PRIMARY KEY,
    role          TEXT NOT NULL CHECK(role IN ('superuser','moderator','regular')),
    owner_name    TEXT NOT NULL,
    owner_email   TEXT,
    token_hash    TEXT NOT NULL UNIQUE,
    username      TEXT UNIQUE,
    password_hash TEXT,
    password_salt TEXT,
    notes         TEXT,
    created_at    INTEGER NOT NULL,
    created_by    INTEGER REFERENCES tokens(id),
    last_used_at  INTEGER,
    revoked       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_tokens_role ON tokens(role);

CREATE TABLE IF NOT EXISTS login_log (
    id         INTEGER PRIMARY KEY,
    token_id   INTEGER,
    username   TEXT NOT NULL,
    role       TEXT,
    success    INTEGER NOT NULL,
    ip         TEXT,
    user_agent TEXT,
    timestamp  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_login_log_ts ON login_log(timestamp);

CREATE TABLE IF NOT EXISTS usage_log (
    id          INTEGER PRIMARY KEY,
    token_id    INTEGER,
    owner_name  TEXT NOT NULL,
    role        TEXT NOT NULL,
    endpoint    TEXT NOT NULL,
    method      TEXT NOT NULL,
    status_code INTEGER,
    ip          TEXT,
    timestamp   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_usage_log_token_ts ON usage_log(token_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_usage_log_ts ON usage_log(timestamp);
";

/// A row from the `tokens` table. `serde(Serialize)` for the admin API, mirroring
/// how the Python routers hand `sqlite3.Row` objects to FastAPI.
#[derive(Debug, Clone, Serialize)]
pub struct Token {
    pub id: i64,
    pub role: String,
    pub owner_name: String,
    pub owner_email: Option<String>,
    #[serde(skip_serializing)] // never expose the hash over the API
    pub token_hash: String,
    pub username: Option<String>,
    #[serde(skip_serializing)]
    pub password_hash: Option<String>,
    #[serde(skip_serializing)]
    pub password_salt: Option<String>,
    pub notes: Option<String>,
    pub created_at: i64,
    pub created_by: Option<i64>,
    pub last_used_at: Option<i64>,
    pub revoked: bool,
}

impl Token {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            role: row.get("role")?,
            owner_name: row.get("owner_name")?,
            owner_email: row.get("owner_email")?,
            token_hash: row.get("token_hash")?,
            username: row.get("username")?,
            password_hash: row.get("password_hash")?,
            password_salt: row.get("password_salt")?,
            notes: row.get("notes")?,
            created_at: row.get("created_at")?,
            created_by: row.get("created_by")?,
            last_used_at: row.get("last_used_at")?,
            revoked: row.get::<_, i64>("revoked")? != 0,
        })
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Port of `get_connection()`: open the auth DB, enable FK enforcement, apply
/// the (idempotent) schema.
pub fn get_connection(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

// ── Hashing ─────────────────────────────────────────────────────────────────

/// SHA-256 hex — fast hash for high-entropy tokens (`hash_token`).
pub fn hash_token(raw_token: &str) -> String {
    let digest = Sha256::digest(raw_token.as_bytes());
    hex::encode(digest)
}

/// PBKDF2-HMAC-SHA256 -> (hash_hex, salt_hex). Generates a 16-byte salt when
/// none is supplied (`hash_password`).
pub fn hash_password(password: &str) -> (String, String) {
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    let hash = hash_password_with_salt(password, &salt);
    (hash, hex::encode(salt))
}

fn hash_password_with_salt(password: &str, salt: &[u8]) -> String {
    let key = pbkdf2::pbkdf2_hmac_array::<Sha256, 32>(password.as_bytes(), salt, PBKDF2_ITERATIONS);
    hex::encode(key)
}

/// Constant-time verify (`verify_password` / `hmac.compare_digest`).
pub fn verify_password(password: &str, password_hash: &str, password_salt: &str) -> bool {
    let salt = match hex::decode(password_salt) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let candidate = hash_password_with_salt(password, &salt);
    candidate.as_bytes().ct_eq(password_hash.as_bytes()).into()
}

/// `secrets.token_urlsafe(32)` — 32 random bytes, URL-safe base64, no padding.
fn generate_raw_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ── CRUD ────────────────────────────────────────────────────────────────────

/// Parameters for `create_token` — a struct instead of Python's kwargs so the
/// many-optionals call site stays readable.
#[derive(Default)]
pub struct NewToken<'a> {
    pub role: &'a str,
    pub owner_name: &'a str,
    pub owner_email: Option<&'a str>,
    pub notes: Option<&'a str>,
    pub username: Option<&'a str>,
    pub password: Option<&'a str>,
    pub created_by: Option<i64>,
}

/// Creates a token record. Returns `(row, raw_token)` — the raw token is only
/// ever available here and at `regenerate_token`; only its hash is stored, so
/// the caller must surface it to the operator immediately.
pub fn create_token(conn: &Connection, new: &NewToken) -> anyhow::Result<(Token, String)> {
    if !matches!(new.role, "superuser" | "moderator" | "regular") {
        anyhow::bail!("invalid role: {:?}", new.role);
    }
    if matches!(new.role, "superuser" | "moderator")
        && !(new.username.is_some() && new.password.is_some())
    {
        anyhow::bail!("{} tokens require a username and password", new.role);
    }

    let raw_token = generate_raw_token();
    let token_hash = hash_token(&raw_token);
    let (password_hash, password_salt) = match new.password {
        Some(p) => {
            let (h, s) = hash_password(p);
            (Some(h), Some(s))
        }
        None => (None, None),
    };

    conn.execute(
        "INSERT INTO tokens (role, owner_name, owner_email, token_hash, username, \
         password_hash, password_salt, notes, created_at, created_by) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        rusqlite::params![
            new.role,
            new.owner_name,
            new.owner_email,
            token_hash,
            new.username,
            password_hash,
            password_salt,
            new.notes,
            now(),
            new.created_by,
        ],
    )?;
    let id = conn.last_insert_rowid();
    let row = get_token(conn, id)?.expect("row just inserted");
    Ok((row, raw_token))
}

pub fn regenerate_token(conn: &Connection, token_id: i64) -> anyhow::Result<(Token, String)> {
    let raw_token = generate_raw_token();
    conn.execute(
        "UPDATE tokens SET token_hash = ?1 WHERE id = ?2",
        rusqlite::params![hash_token(&raw_token), token_id],
    )?;
    let row = get_token(conn, token_id)?
        .ok_or_else(|| anyhow::anyhow!("token {token_id} not found"))?;
    Ok((row, raw_token))
}

/// Fields to update on `edit_token` — `None` leaves a column unchanged.
#[derive(Default)]
pub struct EditToken<'a> {
    pub owner_name: Option<&'a str>,
    pub owner_email: Option<&'a str>,
    pub notes: Option<&'a str>,
    pub revoked: Option<bool>,
    pub username: Option<&'a str>,
    pub password: Option<&'a str>,
}

/// Port of `edit_token`: build a dynamic UPDATE from the supplied fields.
pub fn edit_token(conn: &Connection, token_id: i64, e: &EditToken) -> anyhow::Result<Option<Token>> {
    let mut sets: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(v) = e.owner_name {
        sets.push("owner_name = ?".into());
        params.push(Box::new(v.to_string()));
    }
    if let Some(v) = e.owner_email {
        sets.push("owner_email = ?".into());
        params.push(Box::new(v.to_string()));
    }
    if let Some(v) = e.notes {
        sets.push("notes = ?".into());
        params.push(Box::new(v.to_string()));
    }
    if let Some(v) = e.revoked {
        sets.push("revoked = ?".into());
        params.push(Box::new(v as i64));
    }
    if let Some(v) = e.username {
        sets.push("username = ?".into());
        params.push(Box::new(v.to_string()));
    }
    if let Some(p) = e.password {
        let (hash, salt) = hash_password(p);
        sets.push("password_hash = ?".into());
        params.push(Box::new(hash));
        sets.push("password_salt = ?".into());
        params.push(Box::new(salt));
    }
    if !sets.is_empty() {
        let sql = format!("UPDATE tokens SET {} WHERE id = ?", sets.join(", "));
        params.push(Box::new(token_id));
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        conn.execute(&sql, refs.as_slice())?;
    }
    Ok(get_token(conn, token_id)?)
}

pub fn delete_token(conn: &Connection, token_id: i64) -> rusqlite::Result<bool> {
    // Null out created_by references first (real FK, no ON DELETE) so deleting
    // a token that created others doesn't hit a FK constraint failure.
    conn.execute(
        "UPDATE tokens SET created_by = NULL WHERE created_by = ?1",
        [token_id],
    )?;
    let changed = conn.execute("DELETE FROM tokens WHERE id = ?1", [token_id])?;
    Ok(changed > 0)
}

pub fn list_tokens(conn: &Connection) -> rusqlite::Result<Vec<Token>> {
    let mut stmt = conn.prepare("SELECT * FROM tokens ORDER BY created_at DESC")?;
    let rows = stmt.query_map([], Token::from_row)?;
    rows.collect()
}

pub fn get_token(conn: &Connection, token_id: i64) -> rusqlite::Result<Option<Token>> {
    conn.query_row("SELECT * FROM tokens WHERE id = ?1", [token_id], Token::from_row)
        .optional()
}

// ── Verification (hot path) ─────────────────────────────────────────────────

/// Verify a bearer token; bumps `last_used_at`. Returns the row or `None`
/// (invalid/revoked).
pub fn verify_api_token(conn: &Connection, raw_token: &str) -> rusqlite::Result<Option<Token>> {
    let row = conn
        .query_row(
            "SELECT * FROM tokens WHERE token_hash = ?1 AND revoked = 0",
            [hash_token(raw_token)],
            Token::from_row,
        )
        .optional()?;
    if let Some(ref t) = row {
        conn.execute(
            "UPDATE tokens SET last_used_at = ?1 WHERE id = ?2",
            rusqlite::params![now(), t.id],
        )?;
    }
    Ok(row)
}

/// Console login: username/password against superuser/moderator accounts.
pub fn verify_admin_login(
    conn: &Connection,
    username: &str,
    password: &str,
) -> rusqlite::Result<Option<Token>> {
    let row = conn
        .query_row(
            "SELECT * FROM tokens WHERE username = ?1 AND role IN ('superuser','moderator') AND revoked = 0",
            [username],
            Token::from_row,
        )
        .optional()?;
    let row = match row {
        Some(r) => r,
        None => return Ok(None),
    };
    let (Some(hash), Some(salt)) = (row.password_hash.as_deref(), row.password_salt.as_deref())
    else {
        return Ok(None);
    };
    if !verify_password(password, hash, salt) {
        return Ok(None);
    }
    conn.execute(
        "UPDATE tokens SET last_used_at = ?1 WHERE id = ?2",
        rusqlite::params![now(), row.id],
    )?;
    Ok(Some(row))
}

// ── Usage / login logging ───────────────────────────────────────────────────

pub fn record_usage(
    conn: &Connection,
    token: &Token,
    endpoint: &str,
    method: &str,
    status_code: Option<u16>,
    ip: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO usage_log (token_id, owner_name, role, endpoint, method, status_code, ip, timestamp) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        rusqlite::params![
            token.id,
            token.owner_name,
            token.role,
            endpoint,
            method,
            status_code,
            ip,
            now(),
        ],
    )?;
    Ok(())
}

pub fn record_login(
    conn: &Connection,
    username: &str,
    token: Option<&Token>,
    success: bool,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO login_log (token_id, username, role, success, ip, user_agent, timestamp) \
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![
            token.map(|t| t.id),
            username,
            token.map(|t| t.role.as_str()),
            success as i64,
            ip,
            user_agent,
            now(),
        ],
    )?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct UsageLogEntry {
    pub id: i64,
    pub token_id: Option<i64>,
    pub owner_name: String,
    pub role: String,
    pub endpoint: String,
    pub method: String,
    pub status_code: Option<i64>,
    pub ip: Option<String>,
    pub timestamp: i64,
}

#[derive(Debug, Serialize)]
pub struct LoginLogEntry {
    pub id: i64,
    pub token_id: Option<i64>,
    pub username: String,
    pub role: Option<String>,
    pub success: bool,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub timestamp: i64,
}

pub fn list_usage_log(
    conn: &Connection,
    token_id: Option<i64>,
    limit: i64,
) -> rusqlite::Result<Vec<UsageLogEntry>> {
    let map = |r: &Row| {
        Ok(UsageLogEntry {
            id: r.get("id")?,
            token_id: r.get("token_id")?,
            owner_name: r.get("owner_name")?,
            role: r.get("role")?,
            endpoint: r.get("endpoint")?,
            method: r.get("method")?,
            status_code: r.get("status_code")?,
            ip: r.get("ip")?,
            timestamp: r.get("timestamp")?,
        })
    };
    match token_id {
        Some(tid) => {
            let mut stmt = conn.prepare(
                "SELECT * FROM usage_log WHERE token_id = ?1 ORDER BY timestamp DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![tid, limit], map)?;
            rows.collect()
        }
        None => {
            let mut stmt =
                conn.prepare("SELECT * FROM usage_log ORDER BY timestamp DESC LIMIT ?1")?;
            let rows = stmt.query_map([limit], map)?;
            rows.collect()
        }
    }
}

pub fn list_login_log(conn: &Connection, limit: i64) -> rusqlite::Result<Vec<LoginLogEntry>> {
    let mut stmt = conn.prepare("SELECT * FROM login_log ORDER BY timestamp DESC LIMIT ?1")?;
    let rows = stmt.query_map([limit], |r| {
        Ok(LoginLogEntry {
            id: r.get("id")?,
            token_id: r.get("token_id")?,
            username: r.get("username")?,
            role: r.get("role")?,
            success: r.get::<_, i64>("success")? != 0,
            ip: r.get("ip")?,
            user_agent: r.get("user_agent")?,
            timestamp: r.get("timestamp")?,
        })
    })?;
    rows.collect()
}

pub fn clear_usage_log(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM usage_log", [])
}

pub fn clear_login_log(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM login_log", [])
}

/// One-time legacy migration (`migrate_legacy_admin_credentials`): if no tokens
/// exist and admin_credentials.json is present, seed the first superuser from it.
pub fn migrate_legacy_admin_credentials(
    conn: &Connection,
    username: &str,
    password: &str,
) -> anyhow::Result<bool> {
    let existing: i64 = conn.query_row("SELECT COUNT(*) FROM tokens", [], |r| r.get(0))?;
    if existing > 0 {
        return Ok(false);
    }
    create_token(
        conn,
        &NewToken {
            role: "superuser",
            owner_name: username,
            username: Some(username),
            password: Some(password),
            notes: Some("Migrated automatically from admin_credentials.json on first startup after the token-auth upgrade."),
            ..Default::default()
        },
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    #[test]
    fn token_hash_is_sha256_hex() {
        // Matches Python: hashlib.sha256(b"hello").hexdigest()
        assert_eq!(
            hash_token("hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn password_roundtrips_and_rejects_wrong() {
        let (hash, salt) = hash_password("s3cret");
        assert!(verify_password("s3cret", &hash, &salt));
        assert!(!verify_password("wrong", &hash, &salt));
    }

    #[test]
    fn create_then_verify_api_token() {
        let conn = mem_conn();
        let (row, raw) = create_token(
            &conn,
            &NewToken {
                role: "regular",
                owner_name: "Ada",
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(row.role, "regular");
        // The raw token verifies; a bogus one doesn't.
        assert!(verify_api_token(&conn, &raw).unwrap().is_some());
        assert!(verify_api_token(&conn, "nope").unwrap().is_none());
        // last_used_at got stamped.
        assert!(get_token(&conn, row.id).unwrap().unwrap().last_used_at.is_some());
    }

    #[test]
    fn regular_role_needs_no_creds_but_admin_does() {
        let conn = mem_conn();
        // superuser without username/password is rejected
        let err = create_token(
            &conn,
            &NewToken { role: "superuser", owner_name: "x", ..Default::default() },
        );
        assert!(err.is_err());
    }

    #[test]
    fn admin_login_flow() {
        let conn = mem_conn();
        create_token(
            &conn,
            &NewToken {
                role: "superuser",
                owner_name: "Grace",
                username: Some("grace"),
                password: Some("hopper"),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(verify_admin_login(&conn, "grace", "hopper").unwrap().is_some());
        assert!(verify_admin_login(&conn, "grace", "wrong").unwrap().is_none());
        assert!(verify_admin_login(&conn, "nobody", "hopper").unwrap().is_none());
    }

    #[test]
    fn delete_nulls_created_by_and_removes() {
        let conn = mem_conn();
        let (creator, _) = create_token(
            &conn,
            &NewToken { role: "regular", owner_name: "creator", ..Default::default() },
        )
        .unwrap();
        create_token(
            &conn,
            &NewToken {
                role: "regular",
                owner_name: "child",
                created_by: Some(creator.id),
                ..Default::default()
            },
        )
        .unwrap();
        // Deleting the creator must not fail on the created_by FK.
        assert!(delete_token(&conn, creator.id).unwrap());
        assert_eq!(list_tokens(&conn).unwrap().len(), 1);
    }
}
