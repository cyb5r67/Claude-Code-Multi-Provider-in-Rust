# Document export sidecar: markdown -> .docx / .pdf via pandoc + WeasyPrint.
#
# Three stages. `runtime` is last so `docker compose build` picks it by
# default; build the tests explicitly with `--target test`.

FROM python:3.12-slim AS base

# pandoc does the markdown parsing; the lib* packages are WeasyPrint's
# rendering stack (Pango/Cairo), and DejaVu gives us a Unicode-capable font
# so em dashes and emoji in model output don't blow up PDF rendering.
RUN apt-get update && apt-get install -y --no-install-recommends \
        pandoc \
        libpango-1.0-0 \
        libpangoft2-1.0-0 \
        libharfbuzz0b \
        libcairo2 \
        libgdk-pixbuf-2.0-0 \
        libffi8 \
        fonts-dejavu-core \
        fonts-noto-color-emoji \
        curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY app.py pdf.css ./

FROM base AS test
COPY requirements-dev.txt .
RUN pip install --no-cache-dir -r requirements-dev.txt
COPY test_app.py .
CMD ["pytest", "-q"]

FROM base AS runtime
ENV EXPORT_DIR=/tmp/exports
EXPOSE 8000
CMD ["uvicorn", "app:app", "--host", "0.0.0.0", "--port", "8000"]
