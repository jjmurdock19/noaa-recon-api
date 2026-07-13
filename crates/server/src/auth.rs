//! Auth for the admin console (signed-cookie session) and the optional public
//! API token gate — port of `app/auth.py`.
//!
//! Two mechanisms:
//!   * **Console session** — a signed cookie (axum-extra `SignedCookieJar`, the
//!     analog of Starlette's `SessionMiddleware`) holding `{authenticated, role,
//!     username}`, signed with the secret in `admin_credentials.json`.
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

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub authenticated: bool,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub token_id: Option<i64>,
}

impl Session {
    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }
    pub fn is_superuser(&self) -> bool {
        self.role.as_deref() == Some("superuser")
    }
}

/// Read the session out of the (verified) signed cookie jar. Unsigned/absent ->
/// a default (unauthenticated) session.
pub fn read_session(jar: &SignedCookieJar) -> Session {
    jar.get(SESSION_COOKIE)
        .and_then(|c| serde_json::from_str::<Session>(c.value()).ok())
        .unwrap_or_default()
}

/// Console dependency: 401 unless the session is authenticated (`require_login`).
pub fn require_login(jar: &SignedCookieJar) -> Result<Session, crate::error::ApiError> {
    let s = read_session(jar);
    if s.is_authenticated() {
        Ok(s)
    } else {
        Err(crate::error::ApiError::new(
            axum::http::StatusCode::UNAUTHORIZED,
            "Not authenticated",
        ))
    }
}

/// Console dependency: 401 if not logged in, 403 if not a superuser
/// (`require_superuser`).
pub fn require_superuser(jar: &SignedCookieJar) -> Result<Session, crate::error::ApiError> {
    let s = require_login(jar)?;
    if s.is_superuser() {
        Ok(s)
    } else {
        Err(crate::error::ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "Superuser access required",
        ))
    }
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
