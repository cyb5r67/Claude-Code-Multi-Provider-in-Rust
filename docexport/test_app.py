"""Tests for the document export sidecar.

Run inside the test image so pandoc and WeasyPrint are present:

    docker build --target test -t docexport-test docexport
    docker run --rm docexport-test
"""

import io
import zipfile

import pytest
from fastapi.testclient import TestClient

import app as appmod

client = TestClient(appmod.app)

SAMPLE = """# Quarterly Report

Revenue was **up sharply**, driven by *retention*.

- first item
- second item
  - nested item

| Metric | Value |
|--------|-------|
| Revenue | 42 |

```python
print("hello")
```

> A quoted remark.
"""


@pytest.fixture(autouse=True)
def reset_retention():
    """Each test starts from the default retention window."""
    original = appmod.RETENTION_SECONDS
    yield
    appmod.RETENTION_SECONDS = original


def convert(markdown=SAMPLE, fmt="docx", title=None):
    payload = {"markdown": markdown, "format": fmt}
    if title is not None:
        payload["title"] = title
    return client.post("/convert", json=payload)


def test_health_reports_ok():
    resp = client.get("/health")
    assert resp.status_code == 200
    assert resp.json()["status"] == "ok"


def test_convert_docx_returns_a_downloadable_document():
    resp = convert(fmt="docx")
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["filename"].endswith(".docx")
    assert body["bytes"] > 0

    download = client.get(body["path"])
    assert download.status_code == 200
    # .docx is a zip archive: check the magic bytes rather than trusting pandoc.
    assert download.content[:2] == b"PK"
    assert "attachment" in download.headers["content-disposition"]


def test_convert_pdf_returns_a_downloadable_document():
    resp = convert(fmt="pdf")
    assert resp.status_code == 200, resp.text
    download = client.get(resp.json()["path"])
    assert download.status_code == 200
    assert download.content[:5] == b"%PDF-"


def test_docx_preserves_headings_lists_tables_and_code():
    resp = convert(fmt="docx")
    content = client.get(resp.json()["path"]).content
    xml = zipfile.ZipFile(io.BytesIO(content)).read("word/document.xml").decode("utf-8")

    # Text survives the conversion.
    assert "Quarterly Report" in xml
    assert "first item" in xml
    assert "Revenue" in xml
    assert "42" in xml
    # Syntax highlighting splits code across runs, so match tokens not spans.
    assert "print" in xml
    assert "hello" in xml

    # Structure survives too -- not just a wall of paragraphs.
    assert "Heading1" in xml, "level-1 heading should map to a Word heading style"
    assert "<w:tbl>" in xml, "markdown table should become a real Word table"
    assert "<w:numPr>" in xml, "bullet list should become a real Word list"
    assert "SourceCode" in xml, "fenced code should become a styled code block"


def test_pdf_preserves_document_text():
    from pypdf import PdfReader

    resp = convert(fmt="pdf")
    content = client.get(resp.json()["path"]).content
    text = "\n".join(page.extract_text() for page in PdfReader(io.BytesIO(content)).pages)

    assert "Quarterly Report" in text
    assert "first item" in text
    assert "42" in text


def test_title_becomes_a_safe_filename():
    resp = convert(title="My/Report: v2")
    assert resp.status_code == 200
    filename = resp.json()["filename"]
    assert filename == "My-Report-v2.docx"
    assert "/" not in filename


def test_untitled_requests_get_a_default_filename():
    assert convert().json()["filename"] == "response.docx"


def test_unicode_titles_keep_their_letters():
    # Stripping accents turns "Résumé Q3" into "R-sum-Q3", which is unusable
    # for anyone not writing in English.
    assert convert(title="Résumé Q3").json()["filename"] == "Résumé-Q3.docx"


def test_path_traversal_in_a_title_is_neutralised():
    filename = convert(title="../../etc/passwd").json()["filename"]
    assert "/" not in filename and ".." not in filename


def test_emoji_renders_with_an_embedded_emoji_font():
    # Model output is full of emoji. Without an emoji-capable font WeasyPrint
    # drops them silently, which is worse than drawing a placeholder box.
    from pypdf import PdfReader

    resp = convert(markdown="Status: ✅ shipped", fmt="pdf")
    content = client.get(resp.json()["path"]).content

    fonts = set()
    for page in PdfReader(io.BytesIO(content)).pages:
        for ref in (page.get("/Resources", {}).get("/Font", {}) or {}).values():
            fonts.add(str(ref.get_object().get("/BaseFont", "")))

    assert any("emoji" in name.lower() for name in fonts), f"fonts embedded: {fonts}"


def test_raw_html_tags_are_stripped():
    resp = convert(markdown="<b>bold</b> and <script>alert(1)</script>")
    content = client.get(resp.json()["path"]).content
    xml = zipfile.ZipFile(io.BytesIO(content)).read("word/document.xml").decode("utf-8")

    # Tag text is kept as ordinary content; the tags themselves never reach
    # the document, escaped or otherwise.
    assert "alert(1)" in xml
    assert "bold" in xml
    assert "&lt;script&gt;" not in xml
    assert "&lt;b&gt;" not in xml


def test_images_become_links_rather_than_embedded_fetches():
    # pandoc would otherwise fetch and embed remote images, turning model
    # output into outbound requests from inside the Docker network.
    resp = convert(markdown="![a diagram](https://example.invalid/x.png)")
    content = client.get(resp.json()["path"]).content
    xml = zipfile.ZipFile(io.BytesIO(content)).read("word/document.xml").decode("utf-8")

    assert "a diagram" in xml, "alt text should survive as a link label"
    assert "<w:drawing>" not in xml, "nothing should have been fetched or embedded"


def test_unknown_format_is_rejected():
    resp = convert(fmt="rtf")
    assert resp.status_code == 422


def test_empty_markdown_is_rejected():
    resp = convert(markdown="   \n  ")
    assert resp.status_code == 400
    assert "empty" in resp.json()["detail"].lower()


def test_oversized_input_is_rejected():
    resp = convert(markdown="x" * (appmod.MAX_INPUT_BYTES + 1))
    assert resp.status_code == 413


def test_unknown_file_id_returns_404():
    assert client.get("/files/deadbeef/response.docx").status_code == 404


def test_expired_files_are_pruned():
    path = convert().json()["path"]
    assert client.get(path).status_code == 200

    appmod.RETENTION_SECONDS = 0  # everything is immediately stale
    assert client.get(path).status_code == 404
