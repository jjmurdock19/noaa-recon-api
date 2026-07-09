"""Admin console API — API token / account management, login log, usage
log, and the public-API auth-enabled toggle.

Split out from app/routers/admin.py (which already covers cache/database/
self-update management) since every route here is superuser-only except
/usage-log, which moderators can also see (it's "view data"/"logs"
territory, not user/permission management) — keeping that RBAC-sensitive
surface in its own file makes it easy to review in one place.
"""
from typing import Optional

from fastapi import APIRouter, Depends, HTTPException, Query, Request

from app import auth
from app.services import tokens

router = APIRouter(prefix="/admin", tags=["admin", "tokens"])


def _token_public(row) -> dict:
    """Token row -> API-safe dict — never includes token_hash/password_hash/
    password_salt."""
    return {
        "id": row["id"],
        "role": row["role"],
        "owner_name": row["owner_name"],
        "owner_email": row["owner_email"],
        "username": row["username"],
        "notes": row["notes"],
        "created_at": row["created_at"],
        "created_by": row["created_by"],
        "last_used_at": row["last_used_at"],
        "revoked": bool(row["revoked"]),
    }


# ── Token management (superuser only) ────────────────────────────────────
@router.get("/tokens", dependencies=[Depends(auth.require_superuser)])
async def list_tokens():
    conn = tokens.get_connection()
    try:
        return {"tokens": [_token_public(r) for r in tokens.list_tokens(conn)]}
    finally:
        conn.close()


@router.post("/tokens", dependencies=[Depends(auth.require_superuser)])
async def create_token(request: Request):
    body = await request.json()
    role = str(body.get("role", ""))
    owner_name = str(body.get("owner_name", "")).strip()
    if not owner_name:
        raise HTTPException(400, "owner_name is required")
    if role not in ("superuser", "moderator", "regular"):
        raise HTTPException(400, "role must be one of: superuser, moderator, regular")

    username = body.get("username")
    password = body.get("password")
    if role in ("superuser", "moderator") and not (username and password):
        raise HTTPException(400, f"{role} tokens require a username and password")

    conn = tokens.get_connection()
    try:
        creator_id = request.session.get("token_id")
        try:
            row, raw_token = tokens.create_token(
                conn, role=role, owner_name=owner_name, owner_email=body.get("owner_email"),
                notes=body.get("notes"), username=username, password=password, created_by=creator_id,
            )
        except ValueError as e:
            raise HTTPException(400, str(e)) from e
        except Exception as e:
            if "UNIQUE constraint failed: tokens.username" in str(e):
                raise HTTPException(409, f"username {username!r} is already taken") from e
            raise
        return {**_token_public(row), "token": raw_token}
    finally:
        conn.close()


@router.patch("/tokens/{token_id}", dependencies=[Depends(auth.require_superuser)])
async def edit_token(token_id: int, request: Request):
    body = await request.json()
    conn = tokens.get_connection()
    try:
        if tokens.get_token(conn, token_id) is None:
            raise HTTPException(404, f"No token with id {token_id}")
        row = tokens.edit_token(
            conn, token_id,
            owner_name=body.get("owner_name"), owner_email=body.get("owner_email"),
            notes=body.get("notes"), revoked=body.get("revoked"),
            username=body.get("username"), password=body.get("password"),
        )
        return _token_public(row)
    finally:
        conn.close()


@router.delete("/tokens/{token_id}", dependencies=[Depends(auth.require_superuser)])
async def delete_token(token_id: int):
    conn = tokens.get_connection()
    try:
        if not tokens.delete_token(conn, token_id):
            raise HTTPException(404, f"No token with id {token_id}")
        return {"status": "deleted"}
    finally:
        conn.close()


@router.post("/tokens/{token_id}/regenerate", dependencies=[Depends(auth.require_superuser)])
async def regenerate_token(token_id: int):
    conn = tokens.get_connection()
    try:
        if tokens.get_token(conn, token_id) is None:
            raise HTTPException(404, f"No token with id {token_id}")
        row, raw_token = tokens.regenerate_token(conn, token_id)
        return {**_token_public(row), "token": raw_token}
    finally:
        conn.close()


# ── Login log (superuser only — reveals admin usernames/IPs) ────────────
@router.get("/login-log", dependencies=[Depends(auth.require_superuser)])
async def login_log(limit: int = Query(200, ge=1, le=1000)):
    conn = tokens.get_connection()
    try:
        rows = tokens.list_login_log(conn, limit=limit)
        return {"entries": [dict(r) for r in rows]}
    finally:
        conn.close()


# ── Usage log (superuser + moderator — "view data"/"logs", not permissions) ──
@router.get("/usage-log", dependencies=[Depends(auth.require_login)])
async def usage_log(token_id: Optional[int] = None, limit: int = Query(200, ge=1, le=1000)):
    conn = tokens.get_connection()
    try:
        rows = tokens.list_usage_log(conn, token_id=token_id, limit=limit)
        return {"entries": [dict(r) for r in rows]}
    finally:
        conn.close()


# ── Public-API auth toggle (superuser only) ──────────────────────────────
@router.get("/auth-config", dependencies=[Depends(auth.require_superuser)])
async def get_auth_config():
    return {"enabled": auth.is_auth_enabled()}


@router.post("/auth-config", dependencies=[Depends(auth.require_superuser)])
async def set_auth_config(request: Request):
    body = await request.json()
    if "enabled" not in body:
        raise HTTPException(400, "body must include 'enabled': true|false")
    auth.set_auth_enabled(bool(body["enabled"]))
    return {"enabled": auth.is_auth_enabled()}
