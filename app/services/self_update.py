"""Self-update: pull the latest code from the git remote and restart.

Runs entirely as the unprivileged service user (see
deploy/noaa-recon-api.service, User=server) — no sudo, no root. The trick:
that unit already has Restart=on-failure, so "restarting" is just
deliberately exiting with a non-zero code and letting systemd relaunch
uvicorn, which picks up the freshly-pulled files on the next process start.
There is no in-place code reload.

Safety: only ever a fast-forward pull (`git pull --ff-only`) of a fixed
branch, and only when the working tree is clean. Either check failing
refuses the update with an explicit error instead of merging or discarding
anything — see apply_update().
"""
import datetime
import os
import subprocess
import threading
from typing import Optional

from app.paths import REPO_ROOT

BRANCH = "main"
REMOTE = "origin"

# Serializes all git operations (periodic background check vs. an
# operator-triggered apply) so a fetch/pull never runs concurrently with
# another — git itself would mostly cope, but the check-then-act logic
# below (working-tree-clean check, then pull) needs to see a consistent
# state throughout.
_git_lock = threading.Lock()

_cache_lock = threading.Lock()
_cached_check: dict = {"checked_at": None, "result": None, "error": None}


def _git(*args: str, timeout: int = 60) -> str:
    result = subprocess.run(
        ["git", *args],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    if result.returncode != 0:
        raise RuntimeError((result.stderr or result.stdout).strip() or f"git {' '.join(args)} failed")
    return result.stdout.strip()


def _working_tree_clean() -> bool:
    return _git("status", "--porcelain") == ""


def check_for_update() -> dict:
    """Fetch the remote and report whether the local branch is behind it.
    Read-only — never pulls or modifies the working tree."""
    with _git_lock:
        _git("fetch", REMOTE, BRANCH)
        local = _git("rev-parse", "HEAD")
        remote = _git("rev-parse", f"{REMOTE}/{BRANCH}")
        if local == remote:
            return {"up_to_date": True, "local_commit": local, "remote_commit": remote, "commits_behind": 0, "log": []}
        log = _git("log", "--oneline", f"HEAD..{REMOTE}/{BRANCH}")
        lines = log.splitlines()
        return {
            "up_to_date": False,
            "local_commit": local,
            "remote_commit": remote,
            "commits_behind": len(lines),
            "log": lines,
        }


def get_cached_check() -> dict:
    with _cache_lock:
        return dict(_cached_check)


def set_cached_check(result: Optional[dict], error: Optional[str]) -> None:
    with _cache_lock:
        _cached_check["checked_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        _cached_check["result"] = result
        _cached_check["error"] = error


def apply_update(job: dict) -> None:
    """Pull + reinstall dependencies if pyproject.toml changed, then exit
    so systemd restarts the process on the new code.

    Mutates `job` in place as it progresses (see _self_update_job in
    app/routers/admin.py) so the console can poll status the same way it
    already does for prefetch/archive-update jobs.
    """
    try:
        with _git_lock:
            job["status"] = "checking"
            _git("fetch", REMOTE, BRANCH)
            local_before = _git("rev-parse", "HEAD")
            remote = _git("rev-parse", f"{REMOTE}/{BRANCH}")
            if local_before == remote:
                job["status"] = "up_to_date"
                job["result"] = "Already up to date."
                return

            if not _working_tree_clean():
                raise RuntimeError(
                    "Working tree has uncommitted changes on the server — refusing to pull. "
                    "Resolve manually (git status) before retrying."
                )

            job["status"] = "pulling"
            pyproject_path = REPO_ROOT / "pyproject.toml"
            old_pyproject = pyproject_path.read_text()
            _git("pull", "--ff-only", REMOTE, BRANCH)
            new_commit = _git("rev-parse", "HEAD")
            new_pyproject = pyproject_path.read_text()

        if new_pyproject != old_pyproject:
            job["status"] = "installing_dependencies"
            result = subprocess.run(
                [str(REPO_ROOT / ".venv" / "bin" / "pip"), "install", "-e", "."],
                cwd=REPO_ROOT, capture_output=True, text=True, timeout=600,
            )
            if result.returncode != 0:
                raise RuntimeError(f"pip install failed:\n{(result.stderr or result.stdout)[-4000:]}")

        job["new_commit"] = new_commit
        job["result"] = f"Updated {local_before[:8]} -> {new_commit[:8]}. Restarting…"
        job["status"] = "restarting"
        # Give the HTTP response time to flush back to the caller before the
        # process exits — systemd's Restart=on-failure (see
        # deploy/noaa-recon-api.service) relaunches uvicorn a few seconds
        # later running the code that was just pulled.
        threading.Timer(1.5, lambda: os._exit(1)).start()
    except Exception as e:  # noqa: BLE001 - report to the console, don't crash the background task
        job["status"] = "error"
        job["error"] = str(e)
    finally:
        job["finished_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
