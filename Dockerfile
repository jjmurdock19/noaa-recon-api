FROM python:3.11-slim

WORKDIR /srv/app

# netCDF4 needs libhdf5/libnetcdf at runtime; gcc is needed to build the
# netCDF4 wheel on some platforms.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libhdf5-dev libnetcdf-dev gcc \
    && rm -rf /var/lib/apt/lists/*

COPY pyproject.toml ./
COPY app ./app
RUN pip install --no-cache-dir -e .

EXPOSE 8000
VOLUME ["/srv/app/cache"]

CMD ["uvicorn", "app.main:app", "--host", "0.0.0.0", "--port", "8000"]
