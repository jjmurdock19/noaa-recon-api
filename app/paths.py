from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CACHE_ROOT = REPO_ROOT / "cache"
CACHE_ROOT.mkdir(parents=True, exist_ok=True)
