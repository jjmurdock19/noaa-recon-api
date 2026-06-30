"""Centralized logging — writes to a rotating file under <repo>/logs/ for
long-term monitoring, in addition to whatever stdout/journal capture the
process already has (systemd journal locally, via `journalctl -u
noaa-recon-api`). Without this, app.* loggers (e.g. app/services/goes.py's
`log.info`/`log.exception` calls) and uvicorn's own access/error logs only
ever reach stdout — there was no durable file before this.

The file handler is attached ONLY to the root logger; every other logger
(app.*, noaa_recon_api.*, uvicorn.*) propagates up to root by default, so
attaching it there too would double-log each record. uvicorn's own
"uvicorn.error"/"uvicorn.access" loggers already propagate=True by
default in current uvicorn versions, so root alone is sufficient — this
was verified empirically (attaching to both root and the named uvicorn
loggers produced every line twice).
"""
import logging
import logging.handlers
from pathlib import Path

from app.paths import REPO_ROOT

LOG_DIR = REPO_ROOT / "logs"
LOG_FILE = LOG_DIR / "app.log"


def configure_logging(level: int = logging.INFO) -> None:
    LOG_DIR.mkdir(parents=True, exist_ok=True)

    formatter = logging.Formatter(
        "%(asctime)s %(levelname)-8s %(name)s: %(message)s", datefmt="%Y-%m-%d %H:%M:%S"
    )
    file_handler = logging.handlers.RotatingFileHandler(
        str(LOG_FILE), maxBytes=10 * 1024 * 1024, backupCount=5
    )
    file_handler.setFormatter(formatter)
    file_handler.setLevel(level)

    root = logging.getLogger()
    # Avoid duplicate handlers if this runs more than once in the same
    # process (e.g. `uvicorn --reload` re-imports app.main on reload).
    already_attached = any(
        isinstance(h, logging.handlers.RotatingFileHandler) and h.baseFilename == file_handler.baseFilename
        for h in root.handlers
    )
    if not already_attached:
        root.addHandler(file_handler)
    root.setLevel(level)

    logging.getLogger(__name__).info("Logging configured -> %s (rotating, 10MB x5)", LOG_FILE)
