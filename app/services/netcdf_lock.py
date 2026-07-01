"""Process-wide lock serializing all netCDF4/HDF5 file access.

netCDF4-python (a thin wrapper over the HDF5 C library) is not safe to
call concurrently from multiple threads in the same process — HDF5's
internal state isn't guaranteed re-entrant across threads unless the
library was built with a specific thread-safety option this project
doesn't control (and netCDF4-python doesn't advertise using it even when
available). FastAPI's BackgroundTasks runs synchronous task functions in a
thread pool, so two renders (or a render racing a recon MET harvest, or
two composite-product renders each opening several files) landing at the
same moment run netCDF4 code on different threads simultaneously without
this — reproduced firsthand as a "double free or corruption" process
crash when two composite renders overlapped in dev testing.

Every `netCDF4.Dataset(...)` open/read/close anywhere in this project
must happen inside `with NC_LOCK:` — see app/services/goes.py's
_read_source(), app/services/recon_met.py's process_nc_file() /
extract_storm_from_nc_attrs(), and app/routers/admin.py's
get_goes_nc_info(). The lock only needs to wrap the open/read/close itself
(fast, in-memory), not the S3/HTTP download that precedes it (slow, pure
I/O, no C-library state involved) — so this costs very little real
parallelism for a full elimination of the crash risk.
"""
import threading

NC_LOCK = threading.Lock()
