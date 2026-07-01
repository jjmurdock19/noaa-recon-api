"""Disk-based result cache with a lock-file + TTL pattern.

Mirrors the approach used by proxy.php / goes_tile.py in the hurricanes
site: a `<key>.lock` file marks "rendering in progress" (cleared once
generation finishes or the lock goes stale), and a `<key>.json` file holds
the final result (status: ready|error). FastAPI BackgroundTasks replace the
PHP `nohup` subprocess spawn — everything runs in-process here.
"""
import json
import time
from pathlib import Path
from typing import Optional


class ResultCache:
    def __init__(self, base_dir: Path, lock_timeout: int = 600):
        self.base_dir = base_dir
        self.lock_timeout = lock_timeout
        self.base_dir.mkdir(parents=True, exist_ok=True)

    def _paths(self, key: str) -> tuple[Path, Path]:
        return self.base_dir / f"{key}.json", self.base_dir / f"{key}.lock"

    def get_status(self, key: str) -> Optional[dict]:
        json_path, lock_path = self._paths(key)
        if json_path.exists():
            meta = json.loads(json_path.read_text())
            if meta.get("status") in ("ready", "error"):
                return meta
        if lock_path.exists():
            age = time.time() - lock_path.stat().st_mtime
            if age > self.lock_timeout:
                lock_path.unlink(missing_ok=True)
                return None
            params = {}
            try:
                loaded = json.loads(lock_path.read_text())
                if isinstance(loaded, dict):
                    params = loaded
            except (json.JSONDecodeError, OSError):
                pass  # pre-existing plain-timestamp lock file, or a race with acquire_lock's write
            return {**params, "status": "generating", "key": key, "elapsed": int(age)}
        return None

    def acquire_lock(self, key: str, params: Optional[dict] = None) -> None:
        """`params` (band/cmap/satellite/center/etc.) is whatever the caller
        already knows about the request before rendering starts — persisting
        it here is what lets get_status() report it back while still
        "generating", instead of only once the render finishes (see
        app/routers/satellite.py's acquire_lock() calls)."""
        json_path, lock_path = self._paths(key)
        json_path.unlink(missing_ok=True)
        lock_path.write_text(json.dumps(params or {}))

    def write_result(self, key: str, meta: dict) -> None:
        json_path, lock_path = self._paths(key)
        json_path.write_text(json.dumps(meta))
        lock_path.unlink(missing_ok=True)

    def output_path(self, key: str, suffix: str) -> Path:
        return self.base_dir / f"{key}.{suffix}"

    def list_keys(self) -> list[str]:
        """All keys with a status file (ready/error/generating-via-lock)."""
        json_keys = {p.stem for p in self.base_dir.glob("*.json")}
        lock_keys = {p.stem for p in self.base_dir.glob("*.lock")}
        return sorted(json_keys | lock_keys)

    def delete(self, key: str) -> int:
        """Remove every file for `key` (any suffix). Returns bytes freed."""
        freed = 0
        for p in self.base_dir.glob(f"{key}.*"):
            freed += p.stat().st_size
            p.unlink(missing_ok=True)
        return freed

    def stats(self) -> dict:
        count, total_bytes = 0, 0
        for p in self.base_dir.iterdir():
            if p.is_file():
                count += 1
                total_bytes += p.stat().st_size
        return {"file_count": count, "bytes": total_bytes}
