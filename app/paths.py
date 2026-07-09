from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CACHE_ROOT = REPO_ROOT / "cache"
CACHE_ROOT.mkdir(parents=True, exist_ok=True)

DATA_ROOT = REPO_ROOT / "data"
DATA_ROOT.mkdir(parents=True, exist_ok=True)
STORMS_DB_PATH = DATA_ROOT / "storms.sqlite"
RECON_MET_DB_PATH = DATA_ROOT / "recon_met.sqlite"
AUTH_DB_PATH = DATA_ROOT / "auth.sqlite"
