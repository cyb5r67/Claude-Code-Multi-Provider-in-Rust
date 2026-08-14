# docexport — export chat responses to .docx / .pdf

A small sidecar that turns a markdown chat response into a Word document or a
PDF, plus an Open WebUI Action that puts an **Export Response** button under
every message.

## Why a separate service

Open WebUI Action functions run inside the Open WebUI container. That image has
no pandoc and no PDF rendering stack, and a function's `requirements:` line
installs Python packages only — it cannot install system binaries. Anything you
did manage to install by hand would be wiped by the next Open WebUI image
upgrade.

So the conversion dependencies live here instead. The Action stays a thin HTTP
client, and upgrading Open WebUI can't break your exports.

## Quick start

The service is part of the Compose stack:

```sh
docker compose up -d --build docexport
curl http://localhost:8789/health      # {"status":"ok"}
```

Then install the button:

1. Open WebUI → **Admin Panel → Functions → New Function**.
2. The editor pre-fills a **Filter** scaffold (`class Filter`, with `inlet`
   and `outlet`). That is a placeholder for a different function type —
   select all of it and delete it.
3. Paste [`openwebui_action.py`](openwebui_action.py) in full, including the
   `"""…"""` header block; that is where `title`, `version` and
   `requirements` come from. Open WebUI auto-detects the function type from
   the top-level class name, so `class Action` registers as an Action with no
   further configuration.
4. Save.
5. Back on **Admin Panel → Functions**, make sure the function's **Active**
   toggle is on, then open its **…** menu and switch on **Global**. That
   attaches the button to every model.

   The per-model alternative (**Workspace → Models** → edit a model →
   **Actions**) only works if you have a workspace model to edit. Workspace
   models are presets layered on top of a base model, so that page is empty
   until you create one — and since this proxy deliberately advertises a
   single virtual `big-brother` model, there is usually nothing to gain from
   per-model scoping here. Use **Global**.

An **Export Response** button now appears in the toolbar under each message.
Clicking it appends download links to the chat.

To copy the file to your clipboard:

```powershell
Get-Content docexport\openwebui_action.py -Raw | Set-Clipboard   # Windows
```

```sh
pbcopy < docexport/openwebui_action.py                            # macOS
```

If the button appears but nothing happens, `docker compose logs -f open-webui`
shows the function's traceback; `docker compose exec open-webui curl -fsS
http://docexport:8000/health` confirms the sidecar is reachable from inside
that container.

## Configuration

**Service** (environment variables in `docker-compose.yml`):

| Variable | Default | Purpose |
|----------|---------|---------|
| `RETENTION_SECONDS` | `3600` | How long a generated file stays downloadable |
| `MAX_INPUT_BYTES` | `1048576` | Largest markdown payload accepted |
| `PANDOC_TIMEOUT_SECONDS` | `30` | Per-conversion timeout |
| `EXPORT_DIR` | `/tmp/exports` | Where generated files are written |
| `PDF_CSS` | bundled `pdf.css` | Stylesheet used for PDF rendering |

Set retention from the host with `DOCEXPORT_RETENTION_SECONDS` in `.env`.

**Action** (Valves, editable in the Open WebUI function settings):

| Valve | Default | Purpose |
|-------|---------|---------|
| `service_url` | `http://docexport:8000` | Sidecar address *from the Open WebUI container* |
| `public_base_url` | *(blank)* | Override for the download-link base URL; blank means derive it |
| `public_port` | `8789` | Host port the sidecar is published on |
| `formats` | `docx,pdf` | Which formats to generate; set to one to halve the work |
| `timeout_seconds` | `60` | Client-side give-up |

The Action calls the service over the Docker network, but the download link is
followed by *your browser* over the published port — so the two addresses are
not the same. Rather than hardcode one, the Action reads the `Host` header
(or `X-Forwarded-Host`) of the request that triggered it and builds the link
from that. One install therefore works unchanged from `localhost`, from a LAN
address, and from a hostname.

Set `public_base_url` only for setups that header can't describe — for example
a reverse proxy terminating TLS on a different name. It must include the
scheme and, if non-standard, the port.

## API

`POST /convert`

```json
{ "markdown": "# Title\n\nBody…", "format": "docx", "title": "Optional Title" }
```

```json
{
  "id": "d498c86a…",
  "filename": "Optional-Title.docx",
  "path": "/files/d498c86a…/Optional-Title.docx",
  "bytes": 19211,
  "expires_in": 3600
}
```

`GET /files/{id}/{filename}` returns the document with a
`Content-Disposition: attachment` header. `GET /health` returns
`{"status":"ok"}`.

Errors: `400` empty markdown, `413` oversized input, `422` unknown format,
`404` unknown or expired file, `502` pandoc failure, `504` timeout.

## What conversion actually preserves

pandoc parses GitHub-flavored markdown, so documents keep real structure rather
than becoming a wall of paragraphs:

- headings mapped to Word heading styles / PDF outline levels
- bullet and numbered lists, including nesting
- tables as real tables
- fenced code blocks with syntax highlighting
- blockquotes, links, bold/italic, strikethrough, task lists

PDFs are rendered through WeasyPrint with [`pdf.css`](pdf.css) — A4, page
numbers in the footer, DejaVu for text and Noto Color Emoji for emoji, so
accented characters, typographic dashes, Greek letters and ✅ all render
rather than silently vanishing.

## Deliberate limitations

**Images become links.** pandoc embeds images by fetching them, which would
turn chat content into outbound requests from inside your Docker network.
`![alt](url)` is rewritten to `[alt](url)`: the information survives, the fetch
doesn't.

**Raw HTML is stripped.** The input format is `gfm-raw_html`, so tags never
reach the renderer. Their text content is kept.

**No authentication.** The service trusts anything that can reach it. It binds
to `127.0.0.1` by default; `DOCEXPORT_BIND_ADDR=0.0.0.0` in `.env` exposes it
to the LAN, which is required for downloads to work from any machine other
than the Docker host. Exposed, anyone on your network can convert documents
and fetch any export whose id they know — ids are random UUIDs and files
expire, but there is no login. Do not put it on an untrusted network.

**Files are transient.** They live in the container and vanish on restart or
after `RETENTION_SECONDS`. This is a conversion service, not storage.

## Tests

```sh
docker build --target test -t docexport-test docexport
docker run --rm docexport-test
```

17 tests covering conversion in both formats, structural fidelity (real Word
tables, lists, heading styles, code blocks), PDF text extraction, emoji font
embedding, filename sanitisation including path-traversal attempts, input
validation, and retention expiry.

The test stage is a separate Docker target, so the runtime image doesn't ship
pytest.
