"""In-memory request counters for the admin console's public status panel.

Deliberately not persisted anywhere — this resets on restart, which is fine
since it's a live "calls in the last hour" gauge, not an audit log (the
per-request access log in logs/app.log already covers that). Process-local
only; fine for this single-worker uvicorn deployment (see
deploy/noaa-recon-api.service — no multi-worker config to reconcile across).
"""
import collections
import time

_request_times: collections.deque = collections.deque()
_total_requests = 0
_start_time = time.monotonic()


def _prune(now: float) -> None:
    cutoff = now - 3600
    while _request_times and _request_times[0] < cutoff:
        _request_times.popleft()


def record_request() -> None:
    global _total_requests
    now = time.monotonic()
    _request_times.append(now)
    _total_requests += 1
    _prune(now)


def get_public_stats() -> dict:
    now = time.monotonic()
    _prune(now)
    return {
        "healthy": True,
        "uptime_seconds": int(now - _start_time),
        "calls_last_hour": len(_request_times),
        "total_calls": _total_requests,
    }
