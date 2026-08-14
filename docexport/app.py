"""Markdown -> .docx / .pdf conversion sidecar.

Open WebUI's Action functions run inside the Open WebUI container, which has
neither pandoc nor a PDF rendering stack -- and no way to persist system
packages across an image upgrade. This service owns those dependencies
instead: an Action POSTs markdown here and gets back a short-lived download
URL that the browser can follow.

Conversion is pandoc for markdown parsing (so tables, lists, code blocks and
blockquotes survive as real document structure), plus WeasyPrint for PDF
rendering via an intermediate HTML pass.
"""

from __future__ import annotations

import os
import re
import subprocess
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from fastapi import FastAPI, HTTPException
from fastapi.responses import FileResponse
from pydantic import BaseModel, Field

EXPORT_DIR = Path(os.environ.get("EXPORT_DIR", "/tmp/exports"))
CSS_PATH = Path(os.environ.get("PDF_CSS", Path(__file__).with_name("pdf.css")))

# Module-level so tests can adjust them without rebuilding the image.
RETENTION_SECONDS = int(os.environ.get("RETENTION_SECONDS", 3600))
MAX_INPUT_BYTES = int(os.environ.get("MAX_INPUT_BYTES", 1024 * 1024))
PANDOC_TIMEOUT_SECONDS = int(os.environ.get("PANDOC_TIMEOUT_SECONDS", 30))

# GitHub-flavored markdown matches what chat models actually emit. Raw HTML is
# disabled: model output is untrusted input and must not steer the renderer,
# so any tags come through as literal text.
PANDOC_INPUT_FORMAT = "gfm-raw_html"

MEDIA_TYPES = {
    "docx": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "pdf": "application/pdf",
}

# ![alt](url) -> [alt](url). pandoc embeds images by fetching them, which
# would turn a chat response into outbound requests from inside the Docker
# network. Downgrading to a link keeps the information without the fetch.
_IMAGE = re.compile(r"!\[([^\]]*)\]\(([^)]*)\)")

app = FastAPI(title="docexport", version="1.0.0")


@dataclass
class Export:
    path: Path
    filename: str
    media_type: str
    created_at: float


_EXPORTS: dict[str, Export] = {}


class ConvertRequest(BaseModel):
    markdown: str
    format: Literal["docx", "pdf"] = "docx"
    title: str | None = Field(default=None, max_length=200)


def _prune_expired(now: float | None = None) -> None:
    """Drop exports past the retention window. Cheap enough to run inline."""
    now = time.time() if now is None else now
    for file_id, export in list(_EXPORTS.items()):
        if now - export.created_at >= RETENTION_SECONDS:
            export.path.unlink(missing_ok=True)
            del _EXPORTS[file_id]


def _safe_filename(title: str | None, extension: str) -> str:
    """Collapse a free-text title into something safe to put in a URL path.

    Unicode letters are kept -- stripping them to ASCII mangles any title not
    written in English. Separators and dot runs are neutralised so the result
    can never read as a path.
    """
    base = re.sub(r"[^\w.-]+", "-", (title or "").strip())
    base = re.sub(r"\.{2,}", ".", base).strip("-._")
    return f"{base[:60] or 'response'}.{extension}"


def _run_pandoc(markdown: str, args: list[str]) -> bytes:
    try:
        result = subprocess.run(
            ["pandoc", "-f", PANDOC_INPUT_FORMAT, *args],
            input=markdown.encode("utf-8"),
            capture_output=True,
            timeout=PANDOC_TIMEOUT_SECONDS,
        )
    except FileNotFoundError:  # pragma: no cover - image always ships pandoc
        raise HTTPException(500, "pandoc is not installed in this image")
    except subprocess.TimeoutExpired:
        raise HTTPException(504, "conversion timed out")

    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", "replace").strip()[:500]
        raise HTTPException(502, f"pandoc failed: {detail}")
    return result.stdout


def _block_external_resources(url: str, *_args, **_kwargs):
    """WeasyPrint url_fetcher that refuses everything.

    WeasyPrint treats a raising fetcher as "resource unavailable" and simply
    omits it, which is what we want: no network access driven by model output.
    """
    raise ValueError(f"external resource blocked: {url}")


def _render_docx(markdown: str, destination: Path) -> None:
    _run_pandoc(
        markdown,
        ["-t", "docx", "--highlight-style=tango", "-o", str(destination)],
    )


def _render_pdf(markdown: str, title: str, destination: Path) -> None:
    from weasyprint import CSS, HTML

    html = _run_pandoc(
        markdown,
        [
            "-t", "html5",
            "--standalone",
            "--highlight-style=tango",
            "--metadata", f"title={title}",
        ],
    ).decode("utf-8")

    stylesheets = [CSS(filename=str(CSS_PATH))] if CSS_PATH.exists() else []
    # base_url is deliberately omitted so relative paths cannot resolve to
    # files inside the container.
    HTML(string=html, url_fetcher=_block_external_resources).write_pdf(
        target=str(destination), stylesheets=stylesheets
    )


@app.get("/health")
def health() -> dict[str, str]:
    return {"status": "ok"}


@app.post("/convert")
def convert(request: ConvertRequest) -> dict[str, object]:
    if len(request.markdown.encode("utf-8")) > MAX_INPUT_BYTES:
        raise HTTPException(413, f"markdown exceeds {MAX_INPUT_BYTES} bytes")
    if not request.markdown.strip():
        raise HTTPException(400, "markdown is empty")

    _prune_expired()

    markdown = _IMAGE.sub(r"[\1](\2)", request.markdown)
    filename = _safe_filename(request.title, request.format)
    file_id = uuid.uuid4().hex

    EXPORT_DIR.mkdir(parents=True, exist_ok=True)
    destination = EXPORT_DIR / f"{file_id}.{request.format}"

    if request.format == "docx":
        _render_docx(markdown, destination)
    else:
        _render_pdf(markdown, request.title or "Response", destination)

    _EXPORTS[file_id] = Export(
        path=destination,
        filename=filename,
        media_type=MEDIA_TYPES[request.format],
        created_at=time.time(),
    )

    return {
        "id": file_id,
        "filename": filename,
        "path": f"/files/{file_id}/{filename}",
        "bytes": destination.stat().st_size,
        "expires_in": RETENTION_SECONDS,
    }


@app.get("/files/{file_id}/{filename}")
def download(file_id: str, filename: str) -> FileResponse:
    _prune_expired()

    export = _EXPORTS.get(file_id)
    if export is None or not export.path.exists():
        raise HTTPException(404, "export not found or expired")

    return FileResponse(
        path=export.path,
        media_type=export.media_type,
        filename=export.filename,
    )
