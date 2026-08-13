//! Sentinel detection over model responses (Phase 1 cascade).

use bytes::Bytes;
use serde_json::Value;

/// Result of inspecting the leading text of a local-tier response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentinelVerdict {
    /// The response begins with the sentinel: escalate.
    Sentinel,
    /// The response cannot begin with the sentinel: pass through.
    Clean,
    /// Not enough text yet to decide.
    Undetermined,
}

/// Decide whether `accumulated` (the response text so far) begins with the
/// sentinel. Leading whitespace is ignored; anything after the first token is
/// ordinary content (a sentinel appearing mid-text never escalates).
pub fn check_sentinel(accumulated: &str, sentinel: &str) -> SentinelVerdict {
    let text = accumulated.trim_start();
    if text.starts_with(sentinel) {
        SentinelVerdict::Sentinel
    } else if sentinel.starts_with(text) {
        SentinelVerdict::Undetermined
    } else {
        SentinelVerdict::Clean
    }
}

/// Incrementally scans an Anthropic-format SSE byte stream for the sentinel,
/// buffering every raw chunk so a clean stream can be released to the client
/// unmodified.
///
/// Note: chunks are decoded lossily per-chunk; a multi-byte character split
/// across a chunk boundary may be mangled in the *scanned text* only. The
/// sentinel is ASCII and appears first, so detection is unaffected, and the
/// client always receives the untouched raw bytes.
pub struct SseTextScanner {
    sentinel: String,
    raw: Vec<Bytes>,
    pending: String,
    text: String,
    /// Latches the FIRST verdict reached while draining events, evaluated
    /// per-event rather than once per chunk. The sentinel must be the very
    /// first token of the response, so whichever resolves first —
    /// completion of the sentinel, or a non-text block that rules it out
    /// (a non-text first block, or a partial text prefix immediately
    /// followed by a non-text block) — wins for good, even if both occur
    /// within the same `push()` call. Once set, later events cannot change
    /// it.
    resolved: Option<SentinelVerdict>,
}

impl SseTextScanner {
    pub fn new(sentinel: String) -> Self {
        SseTextScanner {
            sentinel,
            raw: Vec::new(),
            pending: String::new(),
            text: String::new(),
            resolved: None,
        }
    }

    /// Feed one raw chunk; returns the verdict so far.
    pub fn push(&mut self, chunk: &Bytes) -> SentinelVerdict {
        self.raw.push(chunk.clone());
        self.pending.push_str(&String::from_utf8_lossy(chunk));
        while let Some(newline) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=newline).collect();
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(data) = line.strip_prefix("data: ") {
                if let Ok(event) = serde_json::from_str::<Value>(data) {
                    self.absorb_event(&event);
                }
            }
        }
        self.resolved
            .unwrap_or_else(|| check_sentinel(&self.text, &self.sentinel))
    }

    fn absorb_event(&mut self, event: &Value) {
        let is_non_text_block_start = event.get("type").and_then(Value::as_str)
            == Some("content_block_start")
            && !matches!(
                event.pointer("/content_block/type").and_then(Value::as_str),
                Some("text") | None
            );
        let text = match event.get("type").and_then(Value::as_str) {
            Some("content_block_start") => event.pointer("/content_block/text"),
            Some("content_block_delta") => event.pointer("/delta/text"),
            _ => None,
        };
        if let Some(t) = text.and_then(Value::as_str) {
            self.text.push_str(t);
        }
        if self.resolved.is_none() {
            if is_non_text_block_start {
                self.resolved = Some(SentinelVerdict::Clean);
            } else if check_sentinel(&self.text, &self.sentinel) == SentinelVerdict::Sentinel {
                self.resolved = Some(SentinelVerdict::Sentinel);
            }
        }
    }

    /// All raw chunks fed so far, verbatim, for release to the client.
    pub fn into_buffered(self) -> Vec<Bytes> {
        self.raw
    }
}

/// First text block's content from a non-streaming Messages response body.
pub fn json_first_text(body: &Value) -> Option<&str> {
    body.get("content")?
        .as_array()?
        .iter()
        .find(|b| b.get("type").and_then(Value::as_str) == Some("text"))?
        .get("text")?
        .as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "<<ESCALATE>>";

    #[test]
    fn empty_text_is_undetermined() {
        assert_eq!(check_sentinel("", S), SentinelVerdict::Undetermined);
    }

    #[test]
    fn partial_prefix_is_undetermined() {
        assert_eq!(check_sentinel("<<ESC", S), SentinelVerdict::Undetermined);
    }

    #[test]
    fn exact_sentinel_is_detected() {
        assert_eq!(check_sentinel("<<ESCALATE>>", S), SentinelVerdict::Sentinel);
    }

    #[test]
    fn sentinel_with_trailing_text_is_detected() {
        assert_eq!(
            check_sentinel("<<ESCALATE>> this needs the big model", S),
            SentinelVerdict::Sentinel
        );
    }

    #[test]
    fn leading_whitespace_is_ignored() {
        assert_eq!(
            check_sentinel("\n <<ESCALATE>>", S),
            SentinelVerdict::Sentinel
        );
        assert_eq!(check_sentinel("\n ", S), SentinelVerdict::Undetermined);
    }

    #[test]
    fn ordinary_text_is_clean() {
        assert_eq!(
            check_sentinel("The answer is 4.", S),
            SentinelVerdict::Clean
        );
    }

    #[test]
    fn sentinel_mid_text_is_clean() {
        // Only the FIRST token counts (prompt-injection defense).
        assert_eq!(
            check_sentinel("As the file says: <<ESCALATE>>", S),
            SentinelVerdict::Clean
        );
    }

    use bytes::Bytes;
    use serde_json::json;

    fn sse_line(event: &serde_json::Value) -> String {
        format!("data: {event}\n\n")
    }

    #[test]
    fn scanner_detects_sentinel_in_first_delta() {
        let mut scanner = SseTextScanner::new(S.to_string());
        let start = sse_line(&json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}
        }));
        let delta = sse_line(&json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "<<ESCALATE>>"}
        }));
        assert_eq!(
            scanner.push(&Bytes::from(start)),
            SentinelVerdict::Undetermined
        );
        assert_eq!(scanner.push(&Bytes::from(delta)), SentinelVerdict::Sentinel);
    }

    #[test]
    fn scanner_handles_sentinel_split_across_deltas_and_chunks() {
        let mut scanner = SseTextScanner::new(S.to_string());
        let d1 = sse_line(&json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "<<ESC"}
        }));
        let d2 = sse_line(&json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "ALATE>>"}
        }));
        // Split the second event's bytes mid-line to exercise line buffering.
        let d2_bytes = d2.into_bytes();
        let (head, tail) = d2_bytes.split_at(10);

        assert_eq!(
            scanner.push(&Bytes::from(d1)),
            SentinelVerdict::Undetermined
        );
        assert_eq!(
            scanner.push(&Bytes::copy_from_slice(head)),
            SentinelVerdict::Undetermined
        );
        assert_eq!(
            scanner.push(&Bytes::copy_from_slice(tail)),
            SentinelVerdict::Sentinel
        );
    }

    #[test]
    fn scanner_rules_out_sentinel_on_ordinary_text() {
        let mut scanner = SseTextScanner::new(S.to_string());
        let delta = sse_line(&json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "Hello"}
        }));
        assert_eq!(scanner.push(&Bytes::from(delta)), SentinelVerdict::Clean);
    }

    #[test]
    fn scanner_ignores_non_data_lines_and_bad_json() {
        let mut scanner = SseTextScanner::new(S.to_string());
        let noise = "event: message_start\ndata: {not json}\n\n";
        assert_eq!(
            scanner.push(&Bytes::from(noise)),
            SentinelVerdict::Undetermined
        );
    }

    #[test]
    fn scanner_releases_stream_on_leading_tool_use_block() {
        let mut scanner = SseTextScanner::new(S.to_string());
        let start = sse_line(&json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {}}
        }));
        assert_eq!(scanner.push(&Bytes::from(start)), SentinelVerdict::Clean);
    }

    #[test]
    fn scanner_releases_stream_on_leading_thinking_block() {
        let mut scanner = SseTextScanner::new(S.to_string());
        let start = sse_line(&json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "thinking", "thinking": ""}
        }));
        assert_eq!(scanner.push(&Bytes::from(start)), SentinelVerdict::Clean);
    }

    #[test]
    fn scanner_rules_out_partial_prefix_followed_by_non_text_block() {
        let mut scanner = SseTextScanner::new(S.to_string());
        let delta = sse_line(&json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "<<ESC"}
        }));
        assert_eq!(
            scanner.push(&Bytes::from(delta)),
            SentinelVerdict::Undetermined
        );
        let start = sse_line(&json!({
            "type": "content_block_start", "index": 1,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {}}
        }));
        assert_eq!(scanner.push(&Bytes::from(start)), SentinelVerdict::Clean);
    }

    #[test]
    fn scanner_sentinel_followed_by_non_text_block_in_same_chunk_still_escalates() {
        let mut scanner = SseTextScanner::new(S.to_string());
        let delta = sse_line(&json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "<<ESCALATE>>"}
        }));
        let start = sse_line(&json!({
            "type": "content_block_start", "index": 1,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {}}
        }));
        let mut chunk = String::new();
        chunk.push_str(&delta);
        chunk.push_str(&start);
        assert_eq!(scanner.push(&Bytes::from(chunk)), SentinelVerdict::Sentinel);
    }

    #[test]
    fn scanner_returns_all_raw_chunks_verbatim() {
        let mut scanner = SseTextScanner::new(S.to_string());
        let c1 = Bytes::from("event: message_start\n");
        let c2 = Bytes::from("data: {\"type\":\"message_start\"}\n\n");
        scanner.push(&c1);
        scanner.push(&c2);
        assert_eq!(scanner.into_buffered(), vec![c1, c2]);
    }

    #[test]
    fn json_first_text_reads_first_text_block() {
        let body = json!({
            "content": [
                {"type": "text", "text": "<<ESCALATE>>"},
                {"type": "text", "text": "ignored"}
            ]
        });
        assert_eq!(json_first_text(&body), Some("<<ESCALATE>>"));
        assert_eq!(json_first_text(&json!({"content": []})), None);
        assert_eq!(json_first_text(&json!({"no": "content"})), None);
    }
}
