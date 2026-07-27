//! Auth for the admin console (signed-cookie session) and the optional public
//! API token gate.
//!
//! Two mechanisms:
//!   * **Console session** — a signed cookie (axum-extra `SignedCookieJar`)
//!     holding `{authenticated, username, token_id}`, signed with the secret in
//!     `admin_credentials.json`. The cookie carries *identity only*: what the
//!     account may do is re-read from the database on every admin request
//!     (`current_user`), so granting or revoking a permission takes effect on
//!     that person's next click rather than at their next login.
//!   * **Public API gate** — `require_api_token`, opt-in via `auth_config.json`
//!     (off by default). When on, `/v1/*` data routes need a valid
//!     `Authorization: Bearer <token>` OR a logged-in console session.

use std::path::Path;

use axum_extra::extract::cookie::{Cookie, Key, SameSite, SignedCookieJar};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};

const SESSION_COOKIE: &str = "session";

// ── Credentials file (secret key + legacy bootstrap) ────────────────────────

#[derive(Serialize, Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
    pub secret_key: String,
}

fn credentials_path(repo_root: &Path) -> std::path::PathBuf {
    repo_root.join("admin_credentials.json")
}

fn auth_config_path(repo_root: &Path) -> std::path::PathBuf {
    repo_root.join("auth_config.json")
}

/// Read (or create with defaults) `admin_credentials.json` — port of
/// `load_credentials`/`_create_default_credentials`.
pub fn load_credentials(repo_root: &Path) -> std::io::Result<Credentials> {
    let path = credentials_path(repo_root);
    if !path.exists() {
        let mut secret = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut secret);
        let creds = Credentials {
            username: "admin".into(),
            password: "password".into(),
            secret_key: hex::encode(secret),
        };
        std::fs::write(&path, serde_json::to_string_pretty(&creds)? + "\n")?;
        return Ok(creds);
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&text)?)
}

pub fn get_secret_key(repo_root: &Path) -> std::io::Result<String> {
    Ok(load_credentials(repo_root)?.secret_key)
}

/// Derive a 64-byte cookie signing key from the secret (SignedCookieJar's `Key`
/// needs ≥ 64 bytes). The scheme need not match Starlette's — the two servers
/// don't share cookies.
pub fn derive_cookie_key(secret: &str) -> Key {
    let digest = Sha512::digest(secret.as_bytes()); // 64 bytes
    Key::from(digest.as_slice())
}

// ── Public-API gate toggle (auth_config.json) ───────────────────────────────

pub fn is_auth_enabled(repo_root: &Path) -> bool {
    let path = auth_config_path(repo_root);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("enabled").and_then(|e| e.as_bool()))
            .unwrap_or(false),
        Err(_) => false,
    }
}

pub fn set_auth_enabled(repo_root: &Path, enabled: bool) -> std::io::Result<()> {
    let body = serde_json::json!({ "enabled": enabled });
    std::fs::write(auth_config_path(repo_root), serde_json::to_string_pretty(&body)? + "\n")
}

// ── Console session ─────────────────────────────────────────────────────────

/// What the signed cookie stores. Identity only — no permissions, no role. See
/// the module docs for why.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub authenticated: bool,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub token_id: Option<i64>,
}

impl Session {
    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }
}

/// Read the session out of the (verified) signed cookie jar. Unsigned/absent ->
/// a default (unauthenticated) session.
pub fn read_session(jar: &SignedCookieJar) -> Session {
    jar.get(SESSION_COOKIE)
        .and_then(|c| serde_json::from_str::<Session>(c.value()).ok())
        .unwrap_or_default()
}

/// Resolve the signed-in operator's live account row. Every admin handler goes
/// through this (directly or via `require_permission`), which is what makes
/// admin work run *as* a known person: the returned row is the identity the
/// handler acts on and attributes its audit entry to.
///
/// 401 if there's no session, or if the account behind it was deleted, revoked,
/// or lost `console.access` since the cookie was issued.
pub fn require_login(state: &AppState, jar: &SignedCookieJar) -> Result<tokens::Token, ApiError> {
    let unauthenticated =
        || ApiError::new(axum::http::StatusCode::UNAUTHORIZED, "Not authenticated");

    let session = read_session(jar);
    if !session.is_authenticated() {
        return Err(unauthenticated());
    }
    let token_id = session.token_id.ok_or_else(unauthenticated)?;
    let conn = tokens::get_connection(&state.paths.auth_db)?;
    tokens::load_session_account(&conn, token_id)?.ok_or_else(unauthenticated)
}

/// Console gate: 401 if not signed in, 403 without `permission`.
pub fn require_permission(
    state: &AppState,
    jar: &SignedCookieJar,
    permission: &str,
) -> Result<tokens::Token, ApiError> {
    let user = require_login(state, jar)?;
    if user.has_permission(permission) {
        Ok(user)
    } else {
        Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            format!("This action requires the '{permission}' permission"),
        ))
    }
}

/// Best-effort client IP for the audit trail. The API runs behind nginx in
/// production, so the socket peer is always 127.0.0.1 — the forwarding headers
/// are the only thing carrying the real caller.
pub fn client_ip(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Return a jar with the session cookie set (signed).
pub fn write_session(jar: SignedCookieJar, session: &Session) -> SignedCookieJar {
    let value = serde_json::to_string(session).unwrap_or_default();
    let mut cookie = Cookie::new(SESSION_COOKIE, value);
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    jar.add(cookie)
}

/// Return a jar with the session cookie removed (logout). The removal cookie
/// must carry the same path the session was set with ("/"), or the client keeps
/// the original.
pub fn clear_session(jar: SignedCookieJar) -> SignedCookieJar {
    let mut cookie = Cookie::from(SESSION_COOKIE);
    cookie.set_path("/");
    jar.remove(cookie)
}

// ── Public API token gate (middleware) ──────────────────────────────────────

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;
use crate::services::tokens;
use crate::state::AppState;

/// Port of `require_api_token`. A no-op when auth is disabled (the default).
/// When enabled, requires a valid `Authorization: Bearer <token>` OR a logged-in
/// console session; records per-token usage with the final status.
pub async fn require_api_token(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    request: Request,
    next: Next,
) -> Response {
    if !is_auth_enabled(&state.paths.repo_root) {
        return next.run(request).await;
    }

    let authz = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if authz.to_ascii_lowercase().starts_with("bearer ") {
        let raw = authz[7..].trim().to_string();
        let token = match tokens::get_connection(&state.paths.auth_db)
            .and_then(|c| tokens::verify_api_token(&c, &raw))
        {
            Ok(Some(t)) => t,
            Ok(None) => return ApiError::new(axum::http::StatusCode::UNAUTHORIZED, "Invalid or revoked API token").into_response(),
            Err(e) => return ApiError::internal(format!("auth db error: {e}")).into_response(),
        };
        // Record usage with the eventual status (main.py's _record_token_usage).
        let path = request.uri().path().to_string();
        let method = request.method().to_string();
        let response = next.run(request).await;
        if let Ok(conn) = tokens::get_connection(&state.paths.auth_db) {
            let _ = tokens::record_usage(&conn, &token, &path, &method, Some(response.status().as_u16()), None);
        }
        return response;
    }

    // No bearer token — a logged-in console session is also authorized.
    if read_session(&jar).is_authenticated() {
        return next.run(request).await;
    }

    ApiError::new(
        axum::http::StatusCode::UNAUTHORIZED,
        "Missing or malformed Authorization header — expected 'Bearer <token>'",
    )
    .into_response()
}
