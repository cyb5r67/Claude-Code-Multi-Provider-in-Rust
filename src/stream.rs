//! Sentinel detection over model responses (Phase 1 cascade).

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
}
