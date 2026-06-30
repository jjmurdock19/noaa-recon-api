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
            return {"status": "generating", "key": key, "elapsed": int(age)}
        return None

    def acquire_lock(self, key: str) -> None:
        json_path, lock_path = self._paths(key)
        json_path.unlink(missing_ok=True)
        lock_path.write_text(str(time.time()))

    def write_result(self, key: str, meta: dict) -> None:
        json_path, lock_path = self._paths(key)
        json_path.write_text(json.dumps(meta))
        lock_path.unlink(missing_ok=True)

    def output_path(self, key: str, suffix: str) -> Path:
        return self.base_dir / f"{key}.{suffix}"
