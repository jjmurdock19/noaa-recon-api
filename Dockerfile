# Rust build of the NOAA recon API (this branch is Rust — the API server *and*
# all ingest are native; there is no Python). Multi-stage: compile the release
# binary (which builds netCDF-C + HDF5 from source), then a slim runtime image.

# ── Build stage ──────────────────────────────────────────────────────────────
FROM rust:1-bookworm AS build
WORKDIR /src

# netcdf crate (static feature) compiles netCDF-C + HDF5 from source -> needs
# cmake + a C toolchain. CFLAGS downgrades permerrors newer GCC raises on
# netCDF-C's older source.
RUN apt-get update && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*
ENV CFLAGS="-Wno-error=incompatible-pointer-types -Wno-error=int-conversion -Wno-error=implicit-function-declaration"

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p noaa-recon-api

# ── Runtime stage ────────────────────────────────────────────────────────────
FROM debian:bookworm-slim
WORKDIR /srv/app

# The binary is statically linked against netCDF/HDF5/SQLite; only libgcc/libc
# from the base image are needed at runtime.
COPY --from=build /src/target/release/noaa-recon-api /usr/local/bin/noaa-recon-api
# Console UI static assets (served by the binary at "/").
COPY app/console ./app/console
COPY llms.txt ./llms.txt

EXPOSE 8000
ENV PORT=8000 NOAA_RECON_HOST=0.0.0.0 NOAA_RECON_REPO_ROOT=/srv/app
VOLUME ["/srv/app/cache", "/srv/app/data"]

# Serve. Ingest is a subcommand: `docker run ... noaa-recon-api ingest-storms`
# (and ingest-recon / clean-nc-cache) — wire those to your scheduler.
CMD ["noaa-recon-api"]
