"""
title: Export Response
author: Big Brother
version: 1.0.0
required_open_webui_version: 0.5.0
requirements: aiohttp
"""

# Adds an "Export Response" button to the message toolbar. Clicking it sends
# the message's markdown to the docexport sidecar, which converts it with
# pandoc and returns short-lived download links.
#
# Install: Admin Panel -> Functions -> New Function -> paste this file.
# The conversion dependencies live in the sidecar, not here, so an Open WebUI
# upgrade never wipes them.

import asyncio
import re
from typing import Optional

import aiohttp
from pydantic import BaseModel, Field

# Markdown ATX heading, used to give the file a meaningful name.
_HEADING = re.compile(r"^\s{0,3}#{1,6}\s+(.+?)\s*#*\s*$", re.MULTILINE)


class Action:
    class Valves(BaseModel):
        service_url: str = Field(
            default="http://docexport:8000",
            description="Sidecar address as seen from the Open WebUI container.",
        )
        public_base_url: str = Field(
            default="http://localhost:8789",
            description=(
                "Sidecar address as seen from your browser. Change this if you "
                "open Open WebUI from another machine."
            ),
        )
        formats: str = Field(
            default="docx,pdf",
            description="Comma-separated formats to generate: docx, pdf, or both.",
        )
        timeout_seconds: int = Field(
            default=60, description="Give up on a conversion after this long."
        )

    def __init__(self):
        self.valves = self.Valves()

    async def action(
        self,
        body: dict,
        __user__: Optional[dict] = None,
        __event_emitter__=None,
        __event_call__=None,
        **kwargs,
    ) -> None:
        if __event_emitter__ is None:  # nothing to report progress to
            return

        markdown = self._message_content(body)
        if not markdown.strip():
            await self._status(__event_emitter__, "Nothing to export.", done=True)
            return

        formats = [f.strip().lower() for f in self.valves.formats.split(",") if f.strip()]
        if not formats:
            await self._status(__event_emitter__, "No formats configured.", done=True)
            return

        title = self._derive_title(markdown)
        await self._status(
            __event_emitter__, f"Converting to {', '.join(formats)}…", done=False
        )

        links, failures = [], []
        try:
            results = await asyncio.gather(
                *(self._convert(markdown, fmt, title) for fmt in formats),
                return_exceptions=True,
            )
        except Exception as exc:  # pragma: no cover - defensive
            await self._status(__event_emitter__, f"Export failed: {exc}", done=True)
            return

        for fmt, result in zip(formats, results):
            if isinstance(result, Exception):
                failures.append(f"{fmt}: {result}")
            else:
                url = self.valves.public_base_url.rstrip("/") + result["path"]
                size = self._human_size(result["bytes"])
                links.append(f"[{result['filename']}]({url}) · {size}")

        if links:
            body_lines = "\n".join(f"- ⬇ {link}" for link in links)
            note = "\n\n---\n\n**Export ready**\n\n" + body_lines + "\n"
            if failures:
                note += "\nFailed: " + "; ".join(failures) + "\n"
            await __event_emitter__({"type": "message", "data": {"content": note}})
            await self._status(__event_emitter__, "Export ready.", done=True)
        else:
            await self._status(
                __event_emitter__, "Export failed: " + "; ".join(failures), done=True
            )

    async def _convert(self, markdown: str, fmt: str, title: str) -> dict:
        timeout = aiohttp.ClientTimeout(total=self.valves.timeout_seconds)
        payload = {"markdown": markdown, "format": fmt, "title": title}
        url = self.valves.service_url.rstrip("/") + "/convert"

        async with aiohttp.ClientSession(timeout=timeout) as session:
            async with session.post(url, json=payload) as response:
                if response.status != 200:
                    detail = (await response.text())[:200]
                    raise RuntimeError(f"HTTP {response.status} {detail}")
                return await response.json()

    @staticmethod
    def _message_content(body: dict) -> str:
        """The message whose button was clicked, falling back to the last one."""
        messages = body.get("messages") or []
        target_id = body.get("id")
        for message in messages:
            if message.get("id") == target_id:
                return message.get("content") or ""
        return (messages[-1].get("content") or "") if messages else ""

    @staticmethod
    def _derive_title(markdown: str) -> str:
        match = _HEADING.search(markdown)
        if match:
            return match.group(1)[:60]
        first_line = next((l.strip() for l in markdown.splitlines() if l.strip()), "")
        return (first_line[:60] or "response").rstrip()

    @staticmethod
    def _human_size(num_bytes: int) -> str:
        if num_bytes < 1024:
            return f"{num_bytes} B"
        if num_bytes < 1024 * 1024:
            return f"{num_bytes / 1024:.0f} KB"
        return f"{num_bytes / (1024 * 1024):.1f} MB"

    @staticmethod
    async def _status(emitter, description: str, *, done: bool) -> None:
        await emitter(
            {"type": "status", "data": {"description": description, "done": done}}
        )
