"""Auth for both the admin console (signed-cookie session) and the public
API (optional bearer-token gate). Two related but distinct mechanisms:

  - Console login: a username/password checked against app/services/
    tokens.py's superuser/moderator accounts, backing a signed-cookie
    session (Starlette's SessionMiddleware). require_login/require_superuser
    below are the two console-side dependencies.
  - Public API gate: require_api_token below, opt-in per deployment via
    auth_config.json's "enabled" flag (off by default — see the installer's
    "Require API tokens for the public data endpoints?" prompt). When on,
    every /v1/* data route except /v1/health requires a valid
    `Authorization: Bearer <token>` header, checked against the same
    tokens table (any role, not just "regular" — a superuser/moderator's
    token doubles as an API key).

The session secret key and the one-time legacy-admin migration both still
read the original gitignored admin_credentials.json at the repo root
(CREDENTIALS_PATH) — that file predates the tokens table and is now only a
bootstrap/session-secret source, not the active credential store. See
app/services/tokens.py's migrate_legacy_admin_credentials().
"""
import json
import secrets
from typing import Optional

from fastapi import HTTPException, Request

from app.paths import REPO_ROOT

CREDENTIALS_PATH = REPO_ROOT / "admin_credentials.json"
AUTH_CONFIG_PATH = REPO_ROOT / "auth_config.json"


def _create_default_credentials() -> dict:
    creds = {
        "username": "admin",
        "password": "password",
        "secret_key": secrets.token_hex(32),
    }
    CREDENTIALS_PATH.write_text(json.dumps(creds, indent=2) + "\n")
    CREDENTIALS_PATH.chmod(0o600)
    return creds


def load_credentials() -> dict:
    if not CREDENTIALS_PATH.exists():
        return _create_default_credentials()
    return json.loads(CREDENTIALS_PATH.read_text())


def get_secret_key() -> str:
    return load_credentials()["secret_key"]


# ── Public-API token gate toggle ────────────────────────────────────────
def is_auth_enabled() -> bool:
    if not AUTH_CONFIG_PATH.exists():
        return False
    try:
        return bool(json.loads(AUTH_CONFIG_PATH.read_text()).get("enabled", False))
    except (json.JSONDecodeError, OSError):
        return False


def set_auth_enabled(enabled: bool) -> None:
    AUTH_CONFIG_PATH.write_text(json.dumps({"enabled": bool(enabled)}, indent=2) + "\n")


# ── Console session ──────────────────────────────────────────────────────
def is_authenticated(request: Request) -> bool:
    return bool(request.session.get("authenticated"))


def require_login(request: Request) -> None:
    """FastAPI dependency: raises 401 if the session isn't authenticated.
    Satisfied by either role (superuser or moderator) — route-specific
    restrictions use require_superuser on top of this."""
    if not is_authenticated(request):
        raise HTTPException(401, "Not authenticated")


def require_superuser(request: Request) -> None:
    """FastAPI dependency for superuser-only routes (token/account
    management, self-update): 401 if not logged in at all, 403 if logged
    in as a moderator."""
    if not is_authenticated(request):
        raise HTTPException(401, "Not authenticated")
    if request.session.get("role") != "superuser":
        raise HTTPException(403, "Superuser access required")


# ── Public API token gate ────────────────────────────────────────────────
def require_api_token(request: Request):
    """FastAPI dependency for the public data routers (satellite, storms,
    recon, tdr, raw — NOT health, which always stays open). A no-op when
    auth is disabled (the default), preserving today's open-access
    behavior. When enabled, requires EITHER:

      - a valid `Authorization: Bearer <token>` header (any role's token),
        for programmatic API consumers; or
      - a logged-in admin console session (superuser/moderator), so an
        operator using the console's own DB viewer / query tools doesn't
        need to also carry a separate bearer token. Without this, every
        console call to a data endpoint (e.g. /v1/storms/years for the
        database browser) would 401 once auth is enabled, and the console's
        generic 401-means-session-expired handler would bounce the operator
        straight back to the login screen right after logging in.

    Stashes the resolved token row (or None for a session-authorized admin)
    on request.state so app/main.py's logging middleware can record
    accurate per-call usage with the eventual response status code."""
    request.state.token_row = None
    if not is_auth_enabled():
        return None

    from app.services import tokens  # local import: avoids a module-load-order cycle

    authorization = request.headers.get("authorization", "")
    if authorization.lower().startswith("bearer "):
        raw_token = authorization[7:].strip()
        conn = tokens.get_connection()
        try:
            row = tokens.verify_api_token(conn, raw_token)
        finally:
            conn.close()
        if row is None:
            raise HTTPException(401, "Invalid or revoked API token")
        request.state.token_row = row
        return row

    # No bearer token — fall back to an authenticated console session. A
    # logged-in operator is already authorized; their session doubles as
    # data-endpoint access so the console works with auth enabled.
    if is_authenticated(request):
        return None

    raise HTTPException(401, "Missing or malformed Authorization header — expected 'Bearer <token>'")
