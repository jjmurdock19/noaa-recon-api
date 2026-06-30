"""Session-based auth for the admin console.

Credentials live in a gitignored JSON file at the repo root
(admin_credentials.json), created with defaults (admin/password) on first
run if missing — edit that file directly to change them. This is a
single-operator admin tool meant to sit behind nginx/HTTPS, not a
multi-user system; a signed-cookie session (via Starlette's
SessionMiddleware) is proportionate to that, not full user management.
"""
import json
import secrets
from pathlib import Path
from typing import Optional

from fastapi import Request

from app.paths import REPO_ROOT

CREDENTIALS_PATH = REPO_ROOT / "admin_credentials.json"


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


def verify_credentials(username: str, password: str) -> bool:
    creds = load_credentials()
    user_ok = secrets.compare_digest(username, creds["username"])
    pass_ok = secrets.compare_digest(password, creds["password"])
    return user_ok and pass_ok


def get_secret_key() -> str:
    return load_credentials()["secret_key"]


def is_authenticated(request: Request) -> bool:
    return bool(request.session.get("authenticated"))


def require_login(request: Request) -> Optional[dict]:
    """FastAPI dependency: raises 401 if the session isn't authenticated."""
    if not is_authenticated(request):
        from fastapi import HTTPException

        raise HTTPException(401, "Not authenticated")
    return None
