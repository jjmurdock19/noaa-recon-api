from typing import Literal, Optional

from pydantic import BaseModel


class TileStatus(BaseModel):
    status: Literal["ready", "generating", "error", "idle"]
    key: Optional[str] = None
    png_url: Optional[str] = None
    bounds: Optional[list[list[float]]] = None
    band: Optional[int] = None
    cmap: Optional[str] = None
    satellite: Optional[str] = None
    sat_lon: Optional[float] = None
    scan_start: Optional[str] = None
    elapsed: Optional[int] = None
    message: Optional[str] = None
    center: Optional[list[float]] = None
    width_km: Optional[float] = None
    resolution_km: Optional[float] = None
