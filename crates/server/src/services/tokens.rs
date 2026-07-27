//! API token / admin-account store.
//!
//! Backs both the optional public-API token gate (`auth::require_api_token`)
//! and the admin console's per-person login.
//!
//! **Access model — explicit permissions, no role groups.** An account is either
//! an API-only key (no `username`, no console access) or a console account
//! (username + password) whose console capabilities are exactly the rows granted
//! to it in `token_permissions`. The single exception is `is_superuser`, an
//! implicit grant of every permission that survives new permissions being added
//! later — reserved for the operator account (`jjmurdock`). The old
//! `role IN ('superuser','moderator','regular')` column is gone; `migrate`
//! converts each legacy role into the equivalent explicit grants (see
//! `LEGACY_MODERATOR_PERMISSIONS`) and then rebuilds the table without it.
//!
//! Hashing: tokens are SHA-256 hex (fast; they're high-entropy). Passwords are
//! PBKDF2-HMAC-SHA256, 310_000 iters, 16-byte salt (slow KDF).
//! Fresh connection per call, mirroring the per-request connection model used
//! across the rest of the server.

use std::collections::HashMap;
use std::path::Path;

use base64::Engine;
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, Row};
use serde::Serialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// OWASP's current PBKDF2-HMAC-SHA256 baseline.
const PBKDF2_ITERATIONS: u32 = 310_000;

// ── Permission catalog ──────────────────────────────────────────────────────

/// Every permission the console understands, with the human label the account
/// editor shows next to its checkbox. This slice is the single source of truth:
/// the API serves it to the console (`GET /v1/admin/permissions`) so the UI
/// never carries its own copy, and `set_permissions` rejects anything not in it.
pub const PERMISSIONS: &[(&str, &str)] = &[
    ("console.access", "Log in to the admin console"),
    ("status.view", "View system status and database stats"),
    ("logs.view", "View the application log"),
    ("db.view", "Browse database contents"),
    ("cache.view", "View cached tiles and raw netCDF files"),
    ("cache.delete", "Delete cached tiles and raw netCDF files"),
    ("archive.update", "Trigger storm / recon MET / TDR ingest jobs"),
    ("tiles.render", "Submit render queries and prefetch jobs"),
    ("selfupdate.check", "Check whether an API update is available"),
    ("selfupdate.apply", "Apply an update and restart the API"),
    ("users.view", "View accounts and API tokens"),
    ("users.manage", "Create, edit, delete and regenerate accounts and tokens"),
    ("authconfig.manage", "Turn public API authentication on and off"),
    ("loginlog.view", "View the login log"),
    ("logs.clear", "Clear the login and usage logs"),
];

pub fn is_known_permission(perm: &str) -> bool {
    PERMISSIONS.iter().any(|(k, _)| *k == perm)
}

pub fn all_permissions() -> Vec<String> {
    PERMISSIONS.iter().map(|(k, _)| (*k).to_string()).collect()
}

/// What a legacy `moderator` could actually do before the migration, derived
/// from the endpoint gates as they stood: everything behind `require_login`
/// except log clearing. Deliberately *excludes* `logs.clear` — the old build let
/// a moderator wipe the usage log via `require_login`, and that log is now the
/// audit trail, so clearing it is a superuser-grade action.
pub const LEGACY_MODERATOR_PERMISSIONS: &[&str] = &[
    "console.access",
    "status.view",
    "logs.view",
    "db.view",
    "cache.view",
    "cache.delete",
    "archive.update",
    "tiles.render",
];

/// The operator account that keeps the implicit `is_superuser` flag through the
/// migration. Every other legacy superuser is converted to explicit grants.
const RESERVED_SUPERUSER: &str = "jjmurdock";

// ── Schema ──────────────────────────────────────────────────────────────────

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tokens (
    id            INTEGER PRIMARY KEY,
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
    revoked       INTEGER NOT NULL DEFAULT 0,
    is_superuser  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_tokens_username ON tokens(username);

CREATE TABLE IF NOT EXISTS token_permissions (
    token_id   INTEGER NOT NULL REFERENCES tokens(id) ON DELETE CASCADE,
    permission TEXT NOT NULL,
    granted_at INTEGER NOT NULL,
    granted_by INTEGER,
    PRIMARY KEY (token_id, permission)
);

CREATE TABLE IF NOT EXISTS login_log (
    id         INTEGER PRIMARY KEY,
    token_id   INTEGER,
    username   TEXT NOT NULL,
    success    INTEGER NOT NULL,
    ip         TEXT,
    user_agent TEXT,
    timestamp  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_login_log_ts ON login_log(timestamp);

CREATE TABLE IF NOT EXISTS usage_log (
    id          INTEGER PRIMARY KEY,
    source      TEXT NOT NULL DEFAULT 'api',
    token_id    INTEGER,
    owner_name  TEXT NOT NULL,
    username    TEXT,
    action      TEXT,
    detail      TEXT,
    endpoint    TEXT NOT NULL,
    method      TEXT NOT NULL,
    status_code INTEGER,
    ip          TEXT,
    timestamp   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_usage_log_token_ts ON usage_log(token_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_usage_log_ts ON usage_log(timestamp);
";

// ── Rows ────────────────────────────────────────────────────────────────────

/// A row from the `tokens` table plus its explicit grants. `permissions` is
/// filled by every read path here (`get_token`, `list_tokens`,
/// `verify_api_token`, `verify_admin_login`) so callers never see a row whose
/// permission set is silently empty.
#[derive(Debug, Clone, Serialize)]
pub struct Token {
    pub id: i64,
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
    pub is_superuser: bool,
    pub permissions: Vec<String>,
}

impl Token {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
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
            is_superuser: row.get::<_, i64>("is_superuser")? != 0,
            permissions: Vec::new(),
        })
    }

    /// The one place permission questions are answered. Superusers hold every
    /// permission implicitly, including ones added after their account was made.
    pub fn has_permission(&self, permission: &str) -> bool {
        self.is_superuser || self.permissions.iter().any(|p| p == permission)
    }

    /// A console account is one with login credentials; API-only keys have none.
    pub fn is_console_account(&self) -> bool {
        self.username.is_some() && self.password_hash.is_some()
    }

    /// Effective permission list for the API — superusers report the full set so
    /// the console can render their checkboxes without special-casing.
    pub fn effective_permissions(&self) -> Vec<String> {
        if self.is_superuser {
            all_permissions()
        } else {
            self.permissions.clone()
        }
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Open the auth DB, enable FK enforcement, apply the (idempotent) schema.
/// Read/write safe on an already-migrated database; the destructive migration
/// lives in `init_db`, which runs once at startup.
pub fn get_connection(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

// ── Hashing ─────────────────────────────────────────────────────────────────

/// SHA-256 hex — fast hash for high-entropy tokens.
pub fn hash_token(raw_token: &str) -> String {
    let digest = Sha256::digest(raw_token.as_bytes());
    hex::encode(digest)
}

/// PBKDF2-HMAC-SHA256 -> (hash_hex, salt_hex), 16-byte random salt.
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

/// Constant-time verify.
pub fn verify_password(password: &str, password_hash: &str, password_salt: &str) -> bool {
    let salt = match hex::decode(password_salt) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let candidate = hash_password_with_salt(password, &salt);
    candidate.as_bytes().ct_eq(password_hash.as_bytes()).into()
}

/// 32 random bytes, URL-safe base64, no padding.
fn generate_raw_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ── Permission storage ──────────────────────────────────────────────────────

fn load_permissions(conn: &Connection, token_id: i64) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT permission FROM token_permissions WHERE token_id = ?1 ORDER BY permission")?;
    let rows = stmt.query_map([token_id], |r| r.get::<_, String>(0))?;
    rows.collect()
}

fn with_permissions(conn: &Connection, mut token: Token) -> rusqlite::Result<Token> {
    token.permissions = load_permissions(conn, token.id)?;
    Ok(token)
}

/// Replace an account's grants with exactly `permissions`. Unknown keys are
/// rejected rather than stored, so a typo can't create a permission that no
/// gate will ever check. Returns the stored (sorted, de-duplicated) set.
pub fn set_permissions(
    conn: &Connection,
    token_id: i64,
    permissions: &[String],
    granted_by: Option<i64>,
) -> anyhow::Result<Vec<String>> {
    let mut wanted: Vec<String> = Vec::new();
    for p in permissions {
        if !is_known_permission(p) {
            anyhow::bail!("unknown permission: {p:?}");
        }
        if !wanted.contains(p) {
            wanted.push(p.clone());
        }
    }
    wanted.sort();

    conn.execute("DELETE FROM token_permissions WHERE token_id = ?1", [token_id])?;
    let ts = now();
    for p in &wanted {
        conn.execute(
            "INSERT INTO token_permissions (token_id, permission, granted_at, granted_by) \
             VALUES (?1,?2,?3,?4)",
            rusqlite::params![token_id, p, ts, granted_by],
        )?;
    }
    Ok(wanted)
}

// ── CRUD ────────────────────────────────────────────────────────────────────

/// Parameters for `create_token` — a struct instead of Python-style kwargs so
/// the many-optionals call site stays readable.
#[derive(Default)]
pub struct NewToken<'a> {
    pub owner_name: &'a str,
    pub owner_email: Option<&'a str>,
    pub notes: Option<&'a str>,
    pub username: Option<&'a str>,
    pub password: Option<&'a str>,
    pub created_by: Option<i64>,
    pub is_superuser: bool,
    pub permissions: Vec<String>,
}

/// Creates an account. Returns `(row, raw_token)` — the raw token is only ever
/// available here and at `regenerate_token`; only its hash is stored, so the
/// caller must surface it to the operator immediately.
pub fn create_token(conn: &Connection, new: &NewToken) -> anyhow::Result<(Token, String)> {
    if new.owner_name.trim().is_empty() {
        anyhow::bail!("owner_name is required");
    }
    let has_login = new.username.is_some() && new.password.is_some();
    if new.username.is_some() != new.password.is_some() {
        anyhow::bail!("a console account needs both a username and a password");
    }
    if !has_login && (new.is_superuser || !new.permissions.is_empty()) {
        anyhow::bail!("console permissions require a username and password");
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
        "INSERT INTO tokens (owner_name, owner_email, token_hash, username, \
         password_hash, password_salt, notes, created_at, created_by, is_superuser) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        rusqlite::params![
            new.owner_name,
            new.owner_email,
            token_hash,
            new.username,
            password_hash,
            password_salt,
            new.notes,
            now(),
            new.created_by,
            new.is_superuser as i64,
        ],
    )?;
    let id = conn.last_insert_rowid();
    set_permissions(conn, id, &new.permissions, new.created_by)?;
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
    pub is_superuser: Option<bool>,
    /// `Some` replaces the whole grant set; `None` leaves it alone.
    pub permissions: Option<Vec<String>>,
    /// Recorded as `granted_by` on any permission rows this edit writes.
    pub actor_id: Option<i64>,
}

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
    if let Some(v) = e.is_superuser {
        sets.push("is_superuser = ?".into());
        params.push(Box::new(v as i64));
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
    if let Some(perms) = &e.permissions {
        set_permissions(conn, token_id, perms, e.actor_id)?;
    }
    Ok(get_token(conn, token_id)?)
}

pub fn delete_token(conn: &Connection, token_id: i64) -> rusqlite::Result<bool> {
    // Null out created_by references first (real FK, no ON DELETE) so deleting
    // an account that created others doesn't hit a FK constraint failure. The
    // account's own grants go with it via ON DELETE CASCADE.
    conn.execute(
        "UPDATE tokens SET created_by = NULL WHERE created_by = ?1",
        [token_id],
    )?;
    let changed = conn.execute("DELETE FROM tokens WHERE id = ?1", [token_id])?;
    Ok(changed > 0)
}

pub fn list_tokens(conn: &Connection) -> rusqlite::Result<Vec<Token>> {
    let mut stmt = conn.prepare("SELECT * FROM tokens ORDER BY created_at DESC")?;
    let mut rows: Vec<Token> = stmt.query_map([], Token::from_row)?.collect::<Result<_, _>>()?;

    // One query for every grant, grouped in memory — the alternative is a
    // per-row SELECT, and this list is rendered on the console's 5s poll.
    let mut grants: HashMap<i64, Vec<String>> = HashMap::new();
    let mut stmt =
        conn.prepare("SELECT token_id, permission FROM token_permissions ORDER BY permission")?;
    let mut q = stmt.query([])?;
    while let Some(r) = q.next()? {
        grants.entry(r.get(0)?).or_default().push(r.get(1)?);
    }
    for row in &mut rows {
        row.permissions = grants.remove(&row.id).unwrap_or_default();
    }
    Ok(rows)
}

pub fn get_token(conn: &Connection, token_id: i64) -> rusqlite::Result<Option<Token>> {
    let row = conn
        .query_row("SELECT * FROM tokens WHERE id = ?1", [token_id], Token::from_row)
        .optional()?;
    match row {
        Some(t) => Ok(Some(with_permissions(conn, t)?)),
        None => Ok(None),
    }
}

/// How many accounts still hold the implicit all-access flag. Used to stop the
/// last superuser being deleted, revoked or demoted.
pub fn count_superusers(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM tokens WHERE is_superuser = 1 AND revoked = 0",
        [],
        |r| r.get(0),
    )
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
    match row {
        Some(t) => {
            conn.execute(
                "UPDATE tokens SET last_used_at = ?1 WHERE id = ?2",
                rusqlite::params![now(), t.id],
            )?;
            Ok(Some(with_permissions(conn, t)?))
        }
        None => Ok(None),
    }
}

/// Console login. Succeeds only for a non-revoked account that has credentials
/// *and* may use the console — `console.access`, or the superuser flag.
pub fn verify_admin_login(
    conn: &Connection,
    username: &str,
    password: &str,
) -> rusqlite::Result<Option<Token>> {
    let row = conn
        .query_row(
            "SELECT * FROM tokens WHERE username = ?1 AND revoked = 0",
            [username],
            Token::from_row,
        )
        .optional()?;
    let row = match row {
        Some(r) => with_permissions(conn, r)?,
        None => return Ok(None),
    };
    let (Some(hash), Some(salt)) = (row.password_hash.as_deref(), row.password_salt.as_deref())
    else {
        return Ok(None);
    };
    if !verify_password(password, hash, salt) {
        return Ok(None);
    }
    if !row.has_permission("console.access") {
        return Ok(None);
    }
    conn.execute(
        "UPDATE tokens SET last_used_at = ?1 WHERE id = ?2",
        rusqlite::params![now(), row.id],
    )?;
    Ok(Some(row))
}

/// Re-read the account behind a live console session. Returns `None` if the
/// account was deleted, revoked, or had `console.access` taken away since the
/// cookie was issued — every admin request goes through this, so a change to an
/// account takes effect on that account's very next click.
pub fn load_session_account(conn: &Connection, token_id: i64) -> rusqlite::Result<Option<Token>> {
    let Some(token) = get_token(conn, token_id)? else {
        return Ok(None);
    };
    if token.revoked || !token.is_console_account() || !token.has_permission("console.access") {
        return Ok(None);
    }
    Ok(Some(token))
}

// ── Usage / login logging ───────────────────────────────────────────────────

/// A public-API call made with a bearer token.
pub fn record_usage(
    conn: &Connection,
    token: &Token,
    endpoint: &str,
    method: &str,
    status_code: Option<u16>,
    ip: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO usage_log (source, token_id, owner_name, username, action, detail, \
         endpoint, method, status_code, ip, timestamp) \
         VALUES ('api',?1,?2,?3,NULL,NULL,?4,?5,?6,?7,?8)",
        rusqlite::params![
            token.id,
            token.owner_name,
            token.username,
            endpoint,
            method,
            status_code,
            ip,
            now(),
        ],
    )?;
    Ok(())
}

/// An action a signed-in operator took on the admin console. Only state-changing
/// actions are recorded — the console polls status/log/cache endpoints every few
/// seconds, so logging reads would bury the trail under its own traffic.
///
/// `action` is a stable dotted verb (`user.create`, `cache.clear_tiles`);
/// `detail` is the human-readable target ("deleted account 'jane' (#7)").
pub fn record_admin_action(
    conn: &Connection,
    actor: &Token,
    action: &str,
    detail: Option<&str>,
    endpoint: &str,
    method: &str,
    ip: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO usage_log (source, token_id, owner_name, username, action, detail, \
         endpoint, method, status_code, ip, timestamp) \
         VALUES ('admin',?1,?2,?3,?4,?5,?6,?7,NULL,?8,?9)",
        rusqlite::params![
            actor.id,
            actor.owner_name,
            actor.username,
            action,
            detail,
            endpoint,
            method,
            ip,
            now(),
        ],
    )?;
    Ok(())
}

/// `record_admin_action` for handlers that don't already hold a connection.
/// Never fails the request it's describing: a broken audit write is logged and
/// swallowed, because refusing to delete a cache entry over an unwritable log
/// would be a worse outcome than a gap in the trail.
pub fn log_admin_action(
    db_path: &Path,
    actor: &Token,
    action: &str,
    detail: Option<&str>,
    endpoint: &str,
    method: &str,
    ip: Option<&str>,
) {
    let result = get_connection(db_path).and_then(|conn| {
        record_admin_action(&conn, actor, action, detail, endpoint, method, ip)
    });
    if let Err(e) = result {
        tracing::warn!("failed to record admin action {action:?}: {e}");
    }
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
        "INSERT INTO login_log (token_id, username, success, ip, user_agent, timestamp) \
         VALUES (?1,?2,?3,?4,?5,?6)",
        rusqlite::params![
            token.map(|t| t.id),
            username,
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
    /// `"api"` (a bearer-token call) or `"admin"` (a console action).
    pub source: String,
    pub token_id: Option<i64>,
    pub owner_name: String,
    pub username: Option<String>,
    pub action: Option<String>,
    pub detail: Option<String>,
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
    pub success: bool,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub timestamp: i64,
}

/// `source`: `Some("admin")`/`Some("api")` filters the feed; `None` returns both.
pub fn list_usage_log(
    conn: &Connection,
    token_id: Option<i64>,
    source: Option<&str>,
    limit: i64,
) -> rusqlite::Result<Vec<UsageLogEntry>> {
    let map = |r: &Row| {
        Ok(UsageLogEntry {
            id: r.get("id")?,
            source: r.get("source")?,
            token_id: r.get("token_id")?,
            owner_name: r.get("owner_name")?,
            username: r.get("username")?,
            action: r.get("action")?,
            detail: r.get("detail")?,
            endpoint: r.get("endpoint")?,
            method: r.get("method")?,
            status_code: r.get("status_code")?,
            ip: r.get("ip")?,
            timestamp: r.get("timestamp")?,
        })
    };
    let mut sql = String::from("SELECT * FROM usage_log WHERE 1=1");
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(tid) = token_id {
        sql.push_str(" AND token_id = ?");
        params.push(Box::new(tid));
    }
    if let Some(s) = source {
        sql.push_str(" AND source = ?");
        params.push(Box::new(s.to_string()));
    }
    sql.push_str(" ORDER BY timestamp DESC, id DESC LIMIT ?");
    params.push(Box::new(limit));

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(refs.as_slice(), map)?;
    rows.collect()
}

pub fn list_login_log(conn: &Connection, limit: i64) -> rusqlite::Result<Vec<LoginLogEntry>> {
    let mut stmt = conn.prepare("SELECT * FROM login_log ORDER BY timestamp DESC, id DESC LIMIT ?1")?;
    let rows = stmt.query_map([limit], |r| {
        Ok(LoginLogEntry {
            id: r.get("id")?,
            token_id: r.get("token_id")?,
            username: r.get("username")?,
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

/// One-time legacy migration: if no accounts exist and admin_credentials.json is
/// present, seed the first superuser from it.
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
            owner_name: username,
            username: Some(username),
            password: Some(password),
            is_superuser: true,
            notes: Some("Migrated automatically from admin_credentials.json on first startup after the token-auth upgrade."),
            ..Default::default()
        },
    )?;
    Ok(true)
}

// ── Startup migration (role groups -> explicit permissions) ─────────────────

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        if r.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Apply the schema and any one-time migrations. Called once at startup, before
/// any handler opens a connection — `get_connection` deliberately does no
/// destructive DDL so a status poll during nightly ingest can't collide with a
/// table rebuild.
pub fn init_db(db_path: &Path) -> anyhow::Result<()> {
    let conn = Connection::open(db_path)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA)?;
    migrate_roles_to_permissions(&conn)?;
    migrate_log_tables(&conn)?;
    Ok(())
}

/// Convert the legacy `role` column into explicit grants, then drop it.
///
/// Mapping: `jjmurdock` (or, if that account doesn't exist, the oldest
/// superuser — so a dev database is never left with nobody who can manage
/// accounts) keeps `is_superuser`; every other superuser is granted all of
/// `PERMISSIONS` explicitly, so individual permissions can be taken away later;
/// moderators get `LEGACY_MODERATOR_PERMISSIONS`; `regular` API keys get none.
fn migrate_roles_to_permissions(conn: &Connection) -> anyhow::Result<()> {
    if !has_column(conn, "tokens", "role")? {
        return Ok(());
    }

    // Read the legacy roles before the table is rebuilt without them.
    let legacy: Vec<(i64, String, Option<String>)> = {
        let mut stmt = conn.prepare("SELECT id, role, username FROM tokens ORDER BY id")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<Result<_, _>>()?
    };

    let reserved = legacy
        .iter()
        .find(|(_, role, username)| {
            role == "superuser"
                && username.as_deref().map(|u| u.eq_ignore_ascii_case(RESERVED_SUPERUSER)).unwrap_or(false)
        })
        .or_else(|| legacy.iter().find(|(_, role, _)| role == "superuser"))
        .map(|(id, _, _)| *id);

    if !has_column(conn, "tokens", "is_superuser")? {
        conn.execute("ALTER TABLE tokens ADD COLUMN is_superuser INTEGER NOT NULL DEFAULT 0", [])?;
    }

    let ts = now();
    for (id, role, _) in &legacy {
        let grants: Vec<String> = match role.as_str() {
            "superuser" if Some(*id) == reserved => {
                conn.execute("UPDATE tokens SET is_superuser = 1 WHERE id = ?1", [id])?;
                Vec::new()
            }
            "superuser" => all_permissions(),
            "moderator" => LEGACY_MODERATOR_PERMISSIONS.iter().map(|p| p.to_string()).collect(),
            _ => Vec::new(),
        };
        for p in grants {
            conn.execute(
                "INSERT OR IGNORE INTO token_permissions (token_id, permission, granted_at, granted_by) \
                 VALUES (?1,?2,?3,NULL)",
                rusqlite::params![id, p, ts],
            )?;
        }
    }

    // Rebuild without `role`. A plain DROP COLUMN can't be used: `role` is named
    // by a CHECK constraint. `legacy_alter_table` stops the RENAME from
    // rewriting token_permissions' REFERENCES clause to point at the temp name.
    conn.execute_batch(
        "PRAGMA foreign_keys=OFF;
         PRAGMA legacy_alter_table=ON;
         BEGIN;
         CREATE TABLE tokens_migrated (
            id            INTEGER PRIMARY KEY,
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
            revoked       INTEGER NOT NULL DEFAULT 0,
            is_superuser  INTEGER NOT NULL DEFAULT 0
         );
         INSERT INTO tokens_migrated
            (id, owner_name, owner_email, token_hash, username, password_hash,
             password_salt, notes, created_at, created_by, last_used_at, revoked, is_superuser)
            SELECT id, owner_name, owner_email, token_hash, username, password_hash,
                   password_salt, notes, created_at, created_by, last_used_at, revoked, is_superuser
            FROM tokens;
         DROP TABLE tokens;
         ALTER TABLE tokens_migrated RENAME TO tokens;
         CREATE INDEX IF NOT EXISTS idx_tokens_username ON tokens(username);
         COMMIT;
         PRAGMA legacy_alter_table=OFF;
         PRAGMA foreign_keys=ON;",
    )?;

    tracing::info!(
        "auth: migrated {} account(s) from role groups to explicit permissions",
        legacy.len()
    );
    Ok(())
}

/// Drop `role` from the two log tables and add the admin-action columns
/// (`source`, `username`, `action`, `detail`) to `usage_log`. History is kept:
/// existing rows carry over as `source='api'`.
fn migrate_log_tables(conn: &Connection) -> anyhow::Result<()> {
    if has_column(conn, "usage_log", "role")? {
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE usage_log_migrated (
                id          INTEGER PRIMARY KEY,
                source      TEXT NOT NULL DEFAULT 'api',
                token_id    INTEGER,
                owner_name  TEXT NOT NULL,
                username    TEXT,
                action      TEXT,
                detail      TEXT,
                endpoint    TEXT NOT NULL,
                method      TEXT NOT NULL,
                status_code INTEGER,
                ip          TEXT,
                timestamp   INTEGER NOT NULL
             );
             INSERT INTO usage_log_migrated
                (id, source, token_id, owner_name, endpoint, method, status_code, ip, timestamp)
                SELECT id, 'api', token_id, owner_name, endpoint, method, status_code, ip, timestamp
                FROM usage_log;
             DROP TABLE usage_log;
             ALTER TABLE usage_log_migrated RENAME TO usage_log;
             CREATE INDEX IF NOT EXISTS idx_usage_log_token_ts ON usage_log(token_id, timestamp);
             CREATE INDEX IF NOT EXISTS idx_usage_log_ts ON usage_log(timestamp);
             COMMIT;",
        )?;
    }
    if has_column(conn, "login_log", "role")? {
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE login_log_migrated (
                id         INTEGER PRIMARY KEY,
                token_id   INTEGER,
                username   TEXT NOT NULL,
                success    INTEGER NOT NULL,
                ip         TEXT,
                user_agent TEXT,
                timestamp  INTEGER NOT NULL
             );
             INSERT INTO login_log_migrated
                (id, token_id, username, success, ip, user_agent, timestamp)
                SELECT id, token_id, username, success, ip, user_agent, timestamp FROM login_log;
             DROP TABLE login_log;
             ALTER TABLE login_log_migrated RENAME TO login_log;
             CREATE INDEX IF NOT EXISTS idx_login_log_ts ON login_log(timestamp);
             COMMIT;",
        )?;
    }
    Ok(())
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

    fn console_account(conn: &Connection, username: &str, perms: &[&str]) -> Token {
        create_token(
            conn,
            &NewToken {
                owner_name: username,
                username: Some(username),
                password: Some("pw"),
                permissions: perms.iter().map(|p| p.to_string()).collect(),
                ..Default::default()
            },
        )
        .unwrap()
        .0
    }

    #[test]
    fn token_hash_is_sha256_hex() {
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
            &NewToken { owner_name: "Ada", ..Default::default() },
        )
        .unwrap();
        assert!(!row.is_console_account());
        assert!(verify_api_token(&conn, &raw).unwrap().is_some());
        assert!(verify_api_token(&conn, "nope").unwrap().is_none());
        assert!(get_token(&conn, row.id).unwrap().unwrap().last_used_at.is_some());
    }

    #[test]
    fn api_key_cannot_hold_console_permissions() {
        let conn = mem_conn();
        let err = create_token(
            &conn,
            &NewToken {
                owner_name: "keyonly",
                permissions: vec!["console.access".into()],
                ..Default::default()
            },
        );
        assert!(err.is_err());
    }

    #[test]
    fn login_requires_console_access_permission() {
        let conn = mem_conn();
        console_account(&conn, "grace", &["console.access"]);
        console_account(&conn, "noconsole", &["status.view"]);
        assert!(verify_admin_login(&conn, "grace", "pw").unwrap().is_some());
        assert!(verify_admin_login(&conn, "grace", "wrong").unwrap().is_none());
        // Credentials are right, but without console.access there's no way in.
        assert!(verify_admin_login(&conn, "noconsole", "pw").unwrap().is_none());
    }

    #[test]
    fn superuser_holds_every_permission_implicitly() {
        let conn = mem_conn();
        let su = create_token(
            &conn,
            &NewToken {
                owner_name: "root",
                username: Some("jjmurdock"),
                password: Some("pw"),
                is_superuser: true,
                ..Default::default()
            },
        )
        .unwrap()
        .0;
        assert!(su.permissions.is_empty());
        for (key, _) in PERMISSIONS {
            assert!(su.has_permission(key), "superuser missing {key}");
        }
        assert_eq!(su.effective_permissions().len(), PERMISSIONS.len());
        assert!(verify_admin_login(&conn, "jjmurdock", "pw").unwrap().is_some());
    }

    #[test]
    fn set_permissions_replaces_and_rejects_unknown() {
        let conn = mem_conn();
        let user = console_account(&conn, "jane", &["console.access", "cache.view"]);
        let stored =
            set_permissions(&conn, user.id, &["cache.delete".into(), "cache.view".into()], None).unwrap();
        assert_eq!(stored, vec!["cache.delete", "cache.view"]);
        let reread = get_token(&conn, user.id).unwrap().unwrap();
        assert!(!reread.has_permission("console.access"));
        assert!(set_permissions(&conn, user.id, &["not.a.permission".into()], None).is_err());
    }

    #[test]
    fn delete_cascades_permissions_and_nulls_created_by() {
        let conn = mem_conn();
        let creator = console_account(&conn, "creator", &["console.access", "users.manage"]);
        create_token(
            &conn,
            &NewToken { owner_name: "child", created_by: Some(creator.id), ..Default::default() },
        )
        .unwrap();
        assert!(delete_token(&conn, creator.id).unwrap());
        assert_eq!(list_tokens(&conn).unwrap().len(), 1);
        let orphans: i64 = conn
            .query_row("SELECT COUNT(*) FROM token_permissions WHERE token_id = ?1", [creator.id], |r| r.get(0))
            .unwrap();
        assert_eq!(orphans, 0);
    }

    #[test]
    fn admin_actions_and_api_calls_share_the_usage_log() {
        let conn = mem_conn();
        let actor = console_account(&conn, "jane", &["console.access"]);
        record_admin_action(&conn, &actor, "user.create", Some("created 'bob'"), "/v1/admin/tokens", "POST", Some("10.0.0.1")).unwrap();
        record_usage(&conn, &actor, "/v1/storms", "GET", Some(200), None).unwrap();

        let all = list_usage_log(&conn, None, None, 50).unwrap();
        assert_eq!(all.len(), 2);
        let admin_only = list_usage_log(&conn, None, Some("admin"), 50).unwrap();
        assert_eq!(admin_only.len(), 1);
        assert_eq!(admin_only[0].action.as_deref(), Some("user.create"));
        assert_eq!(admin_only[0].username.as_deref(), Some("jane"));
    }

    #[test]
    fn session_account_lookup_rejects_revoked_and_demoted() {
        let conn = mem_conn();
        let user = console_account(&conn, "jane", &["console.access"]);
        assert!(load_session_account(&conn, user.id).unwrap().is_some());

        // Losing console.access invalidates the live session on the next request.
        set_permissions(&conn, user.id, &["status.view".into()], None).unwrap();
        assert!(load_session_account(&conn, user.id).unwrap().is_none());

        set_permissions(&conn, user.id, &["console.access".into()], None).unwrap();
        edit_token(&conn, user.id, &EditToken { revoked: Some(true), ..Default::default() }).unwrap();
        assert!(load_session_account(&conn, user.id).unwrap().is_none());
    }

    /// Builds a database on the *old* schema and runs the real migration over
    /// it — the mapping this whole change hinges on.
    #[test]
    fn migration_converts_role_groups_to_explicit_permissions() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tokens (
                id INTEGER PRIMARY KEY,
                role TEXT NOT NULL CHECK(role IN ('superuser','moderator','regular')),
                owner_name TEXT NOT NULL, owner_email TEXT,
                token_hash TEXT NOT NULL UNIQUE, username TEXT UNIQUE,
                password_hash TEXT, password_salt TEXT, notes TEXT,
                created_at INTEGER NOT NULL, created_by INTEGER REFERENCES tokens(id),
                last_used_at INTEGER, revoked INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE usage_log (
                id INTEGER PRIMARY KEY, token_id INTEGER, owner_name TEXT NOT NULL,
                role TEXT NOT NULL, endpoint TEXT NOT NULL, method TEXT NOT NULL,
                status_code INTEGER, ip TEXT, timestamp INTEGER NOT NULL
             );
             CREATE TABLE login_log (
                id INTEGER PRIMARY KEY, token_id INTEGER, username TEXT NOT NULL,
                role TEXT, success INTEGER NOT NULL, ip TEXT, user_agent TEXT,
                timestamp INTEGER NOT NULL
             );
             INSERT INTO tokens (id, role, owner_name, token_hash, username, created_at)
                VALUES (1,'superuser','Josh','h1','jjmurdock',1),
                       (2,'superuser','Other Su','h2','othersu',2),
                       (3,'moderator','Mod','h3','mod',3),
                       (4,'regular','CI Key','h4',NULL,4);
             INSERT INTO usage_log (token_id, owner_name, role, endpoint, method, status_code, timestamp)
                VALUES (4,'CI Key','regular','/v1/storms','GET',200,5);
             INSERT INTO login_log (token_id, username, role, success, timestamp)
                VALUES (1,'jjmurdock','superuser',1,6);",
        )
        .unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        migrate_roles_to_permissions(&conn).unwrap();
        migrate_log_tables(&conn).unwrap();

        assert!(!has_column(&conn, "tokens", "role").unwrap());
        assert!(!has_column(&conn, "usage_log", "role").unwrap());
        assert!(!has_column(&conn, "login_log", "role").unwrap());

        // jjmurdock keeps the implicit flag and holds no explicit rows.
        let josh = get_token(&conn, 1).unwrap().unwrap();
        assert!(josh.is_superuser);
        assert!(josh.permissions.is_empty());
        assert!(josh.has_permission("users.manage"));

        // The other superuser is converted to explicit grants — revocable now.
        let other = get_token(&conn, 2).unwrap().unwrap();
        assert!(!other.is_superuser);
        assert_eq!(other.permissions.len(), PERMISSIONS.len());

        let moderator = get_token(&conn, 3).unwrap().unwrap();
        assert!(!moderator.is_superuser);
        assert_eq!(moderator.permissions.len(), LEGACY_MODERATOR_PERMISSIONS.len());
        assert!(moderator.has_permission("archive.update"));
        assert!(!moderator.has_permission("users.manage"));
        assert!(!moderator.has_permission("logs.clear"));

        // API-only keys get nothing and can't reach the console.
        let key = get_token(&conn, 4).unwrap().unwrap();
        assert!(key.permissions.is_empty());
        assert!(!key.has_permission("console.access"));

        // Log history survives the rebuild, tagged as API traffic.
        let usage = list_usage_log(&conn, None, None, 50).unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].source, "api");
        assert_eq!(list_login_log(&conn, 50).unwrap().len(), 1);

        // Idempotent: a second run over the migrated DB is a no-op.
        migrate_roles_to_permissions(&conn).unwrap();
        migrate_log_tables(&conn).unwrap();
        assert_eq!(list_tokens(&conn).unwrap().len(), 4);
    }
}
