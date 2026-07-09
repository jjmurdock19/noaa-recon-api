"""API token / admin-account store — backs both the optional public-API
token gate (app/auth.py's require_api_token) and the admin console's
per-person login (superuser/moderator accounts replace the old single
shared admin_credentials.json login).

Three roles, one table:
  - "regular"   — a plain API key for the public /v1/* data endpoints,
                  tracked in usage_log. No console login (no
                  username/password on the row).
  - "moderator" — console login (username/password) with full access to
                  everything except token/user management and self-update.
  - "superuser" — console login with unrestricted access, including
                  managing tokens/accounts and triggering self-update.

Same conventions as app/services/storms.py / app/services/recon_met.py:
schema-in-code-string, idempotent CREATE TABLE/INDEX IF NOT EXISTS,
sqlite3.Row row factory, no ORM, explicit conn.close() by callers.
"""
import hashlib
import hmac
import os
import secrets
import sqlite3
import time
from typing import Optional

from app.paths import AUTH_DB_PATH

PBKDF2_ITERATIONS = 310_000  # OWASP's current PBKDF2-HMAC-SHA256 baseline

SCHEMA = """
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
"""


def get_connection() -> sqlite3.Connection:
    conn = sqlite3.connect(str(AUTH_DB_PATH))
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA foreign_keys = ON")
    conn.executescript(SCHEMA)
    return conn


# ── Hashing ──────────────────────────────────────────────────────────────
# Two different secrets, two different strategies (see module docstring's
# sibling design doc / PR description for the reasoning): token secrets are
# high-entropy machine-generated values, so a fast hash (sha256) is fine and
# keeps API-request-path lookups cheap; passwords are human-chosen and need
# a slow KDF to resist offline guessing if the DB ever leaks.
def hash_token(raw_token: str) -> str:
    return hashlib.sha256(raw_token.encode()).hexdigest()


def hash_password(password: str, salt: Optional[bytes] = None) -> tuple[str, str]:
    """Returns (password_hash_hex, salt_hex)."""
    if salt is None:
        salt = os.urandom(16)
    digest = hashlib.pbkdf2_hmac("sha256", password.encode(), salt, PBKDF2_ITERATIONS)
    return digest.hex(), salt.hex()


def verify_password(password: str, password_hash: str, password_salt: str) -> bool:
    salt = bytes.fromhex(password_salt)
    candidate_hash, _ = hash_password(password, salt)
    return hmac.compare_digest(candidate_hash, password_hash)


# ── CRUD ─────────────────────────────────────────────────────────────────
def create_token(
    conn: sqlite3.Connection,
    role: str,
    owner_name: str,
    owner_email: Optional[str] = None,
    notes: Optional[str] = None,
    username: Optional[str] = None,
    password: Optional[str] = None,
    created_by: Optional[int] = None,
) -> tuple[sqlite3.Row, str]:
    """Creates a token record. Returns (row, raw_token) — raw_token is only
    ever available here and at regenerate_token(); the caller must show it
    to the operator immediately, since only its hash is stored."""
    if role not in ("superuser", "moderator", "regular"):
        raise ValueError(f"invalid role: {role!r}")
    if role in ("superuser", "moderator") and not (username and password):
        raise ValueError(f"{role} tokens require a username and password")

    raw_token = secrets.token_urlsafe(32)
    token_hash = hash_token(raw_token)
    password_hash = password_salt = None
    if password is not None:
        password_hash, password_salt = hash_password(password)

    cur = conn.execute(
        "INSERT INTO tokens (role, owner_name, owner_email, token_hash, username, "
        "password_hash, password_salt, notes, created_at, created_by) "
        "VALUES (?,?,?,?,?,?,?,?,?,?)",
        (role, owner_name, owner_email, token_hash, username,
         password_hash, password_salt, notes, int(time.time()), created_by),
    )
    conn.commit()
    row = conn.execute("SELECT * FROM tokens WHERE id = ?", (cur.lastrowid,)).fetchone()
    return row, raw_token


def regenerate_token(conn: sqlite3.Connection, token_id: int) -> tuple[sqlite3.Row, str]:
    raw_token = secrets.token_urlsafe(32)
    conn.execute("UPDATE tokens SET token_hash = ? WHERE id = ?", (hash_token(raw_token), token_id))
    conn.commit()
    row = conn.execute("SELECT * FROM tokens WHERE id = ?", (token_id,)).fetchone()
    return row, raw_token


def edit_token(
    conn: sqlite3.Connection,
    token_id: int,
    owner_name: Optional[str] = None,
    owner_email: Optional[str] = None,
    notes: Optional[str] = None,
    revoked: Optional[bool] = None,
    username: Optional[str] = None,
    password: Optional[str] = None,
) -> Optional[sqlite3.Row]:
    fields, params = [], []
    if owner_name is not None:
        fields.append("owner_name = ?"); params.append(owner_name)
    if owner_email is not None:
        fields.append("owner_email = ?"); params.append(owner_email)
    if notes is not None:
        fields.append("notes = ?"); params.append(notes)
    if revoked is not None:
        fields.append("revoked = ?"); params.append(int(revoked))
    if username is not None:
        fields.append("username = ?"); params.append(username)
    if password is not None:
        password_hash, password_salt = hash_password(password)
        fields.append("password_hash = ?"); params.append(password_hash)
        fields.append("password_salt = ?"); params.append(password_salt)
    if not fields:
        return conn.execute("SELECT * FROM tokens WHERE id = ?", (token_id,)).fetchone()
    params.append(token_id)
    conn.execute(f"UPDATE tokens SET {', '.join(fields)} WHERE id = ?", params)
    conn.commit()
    return conn.execute("SELECT * FROM tokens WHERE id = ?", (token_id,)).fetchone()


def delete_token(conn: sqlite3.Connection, token_id: int) -> bool:
    """Hard delete. login_log/usage_log rows keep their own denormalized
    owner_name/role/username snapshot (see module docstring), so deleting
    a token doesn't erase its history — only conn.execute's FK is a plain
    reference, not ON DELETE CASCADE."""
    cur = conn.execute("DELETE FROM tokens WHERE id = ?", (token_id,))
    conn.commit()
    return cur.rowcount > 0


def list_tokens(conn: sqlite3.Connection) -> list[sqlite3.Row]:
    return conn.execute("SELECT * FROM tokens ORDER BY created_at DESC").fetchall()


def get_token(conn: sqlite3.Connection, token_id: int) -> Optional[sqlite3.Row]:
    return conn.execute("SELECT * FROM tokens WHERE id = ?", (token_id,)).fetchone()


# ── Verification (hot path — called on every gated request) ─────────────
def verify_api_token(conn: sqlite3.Connection, raw_token: str) -> Optional[sqlite3.Row]:
    row = conn.execute(
        "SELECT * FROM tokens WHERE token_hash = ? AND revoked = 0", (hash_token(raw_token),)
    ).fetchone()
    if row is None:
        return None
    conn.execute("UPDATE tokens SET last_used_at = ? WHERE id = ?", (int(time.time()), row["id"]))
    conn.commit()
    return row


def verify_admin_login(conn: sqlite3.Connection, username: str, password: str) -> Optional[sqlite3.Row]:
    row = conn.execute(
        "SELECT * FROM tokens WHERE username = ? AND role IN ('superuser','moderator') AND revoked = 0",
        (username,),
    ).fetchone()
    if row is None or not row["password_hash"]:
        return None
    if not verify_password(password, row["password_hash"], row["password_salt"]):
        return None
    conn.execute("UPDATE tokens SET last_used_at = ? WHERE id = ?", (int(time.time()), row["id"]))
    conn.commit()
    return row


# ── Logging ──────────────────────────────────────────────────────────────
def record_usage(
    conn: sqlite3.Connection, token_row: sqlite3.Row, endpoint: str, method: str,
    status_code: Optional[int], ip: Optional[str],
) -> None:
    conn.execute(
        "INSERT INTO usage_log (token_id, owner_name, role, endpoint, method, status_code, ip, timestamp) "
        "VALUES (?,?,?,?,?,?,?,?)",
        (token_row["id"], token_row["owner_name"], token_row["role"], endpoint, method,
         status_code, ip, int(time.time())),
    )
    conn.commit()


def record_login(
    conn: sqlite3.Connection, username: str, token_row: Optional[sqlite3.Row],
    success: bool, ip: Optional[str], user_agent: Optional[str],
) -> None:
    conn.execute(
        "INSERT INTO login_log (token_id, username, role, success, ip, user_agent, timestamp) "
        "VALUES (?,?,?,?,?,?,?)",
        (token_row["id"] if token_row else None, username, token_row["role"] if token_row else None,
         int(success), ip, user_agent, int(time.time())),
    )
    conn.commit()


def list_usage_log(conn: sqlite3.Connection, token_id: Optional[int] = None, limit: int = 200) -> list[sqlite3.Row]:
    if token_id is not None:
        return conn.execute(
            "SELECT * FROM usage_log WHERE token_id = ? ORDER BY timestamp DESC LIMIT ?", (token_id, limit)
        ).fetchall()
    return conn.execute("SELECT * FROM usage_log ORDER BY timestamp DESC LIMIT ?", (limit,)).fetchall()


def list_login_log(conn: sqlite3.Connection, limit: int = 200) -> list[sqlite3.Row]:
    return conn.execute("SELECT * FROM login_log ORDER BY timestamp DESC LIMIT ?", (limit,)).fetchall()


# ── Legacy migration ─────────────────────────────────────────────────────
def migrate_legacy_admin_credentials(conn: sqlite3.Connection) -> int:
    """One-time-idempotent fixup: if no tokens exist yet and the old
    single-shared-admin admin_credentials.json is present, creates the
    first superuser account from it (password re-hashed from plaintext
    into the new PBKDF2 scheme). Does not touch or delete
    admin_credentials.json — its secret_key is still used to sign the
    session cookie (see app/auth.py). Safe to call on every startup: a
    no-op once any token row exists. Returns 1 if a row was created, 0
    otherwise."""
    from app import auth  # local import: avoids a module-load-order cycle (auth imports tokens too)

    existing = conn.execute("SELECT COUNT(*) FROM tokens").fetchone()[0]
    if existing > 0:
        return 0
    if not auth.CREDENTIALS_PATH.exists():
        return 0

    creds = auth.load_credentials()
    create_token(
        conn,
        role="superuser",
        owner_name=creds["username"],
        notes="Migrated automatically from admin_credentials.json on first startup after the token-auth upgrade.",
        username=creds["username"],
        password=creds["password"],
    )
    return 1
