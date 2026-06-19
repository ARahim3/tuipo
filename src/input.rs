//! Combines the keystroke buffer with the spell engine and caches the result.
//!
//! Owning these together lets later phases stay simple — render code asks
//! `state.issues()` and gets back the current lints without worrying about
//! re-running harper, debouncing, or version bookkeeping.

use crate::buffer::{FeedOutcome, InputBuffer};
use crate::spell::{SpellChecker, SpellIssue};

pub struct InputState {
    buffer: InputBuffer,
    checker: SpellChecker,
    issues: Vec<SpellIssue>,
    last_checked_version: u64,
}

impl InputState {
    pub fn new() -> Self {
        Self {
            buffer: InputBuffer::new(),
            checker: SpellChecker::new(),
            issues: Vec::new(),
            last_checked_version: 0,
        }
    }

    /// Read-only access to the underlying keystroke buffer. Used by phase 3
    /// (cursor position mapping) and phase 4 (replacement injection).
    #[allow(dead_code)]
    pub fn buffer(&self) -> &InputBuffer {
        &self.buffer
    }

    /// Toggle the chunk-level paste flag on the underlying buffer. Set
    /// by the stdin pump (`pty::pump_stdin_to_pty`) before iterating
    /// the bytes of a paste-shaped read chunk and cleared after, so
    /// `\n` inside the chunk is inserted as content rather than
    /// triggering Boundary. Marker-driven `in_paste` is independent.
    /// See `buffer::InputBuffer::set_chunk_paste`.
    pub fn set_chunk_paste(&mut self, flag: bool) {
        self.buffer.set_chunk_paste(flag);
    }

    /// Current lint list. Used by phase 5+ rendering and phase 6 picker.
    #[allow(dead_code)]
    pub fn issues(&self) -> &[SpellIssue] {
        &self.issues
    }

    pub fn feed_bytes(&mut self, bytes: &[u8]) -> FeedOutcome {
        let outcome = self.buffer.feed_bytes(bytes);
        match outcome {
            FeedOutcome::Updated => self.refresh_issues(),
            FeedOutcome::Boundary => {
                self.issues.clear();
                self.last_checked_version = self.buffer.version();
            }
            FeedOutcome::NoChange => {}
        }
        outcome
    }

    /// Feed bytes to the buffer WITHOUT spell-checking each intermediate
    /// prefix. Used for paste bursts: linting on every byte re-parses the
    /// whole buffer through harper, so an N-char paste costs O(N²) harper
    /// passes and stalls the stdin→PTY forwarding for seconds (GH #1). The
    /// caller MUST call [`Self::refresh`] once after the burst so `issues`
    /// reflects the final buffer. Boundary still clears the lint cache
    /// immediately — cheap, and keeps invariants intact if a control byte
    /// happens to land inside a pasted chunk.
    pub fn feed_bytes_deferred(&mut self, bytes: &[u8]) -> FeedOutcome {
        let outcome = self.buffer.feed_bytes(bytes);
        if let FeedOutcome::Boundary = outcome {
            self.issues.clear();
            self.last_checked_version = self.buffer.version();
        }
        outcome
    }

    /// Bring the lint cache in sync with the current buffer. Runs harper
    /// at most once (no-op when the buffer is unchanged since the last
    /// check). Pairs with [`Self::feed_bytes_deferred`].
    pub fn refresh(&mut self) {
        self.refresh_issues();
    }

    fn refresh_issues(&mut self) {
        let v = self.buffer.version();
        if v == self.last_checked_version {
            return;
        }
        self.last_checked_version = v;
        self.issues = self.checker.check(self.buffer.text());
    }
}

impl Default for InputState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_clears_issues() {
        let mut s = InputState::new();
        s.feed_bytes(b"teh ");
        let before = s.issues().len();
        // We expect at least one lint on "teh"
        assert!(before > 0, "expected issues from `teh `, got {before}");
        s.feed_bytes(&[0x0D]);
        assert!(s.issues().is_empty(), "issues survived boundary");
    }

    #[test]
    fn issues_update_on_change() {
        let mut s = InputState::new();
        s.feed_bytes(b"hello");
        let v0 = s.buffer().version();
        s.feed_bytes(b" teh");
        assert_ne!(v0, s.buffer().version());
    }

    #[test]
    fn misspelling_is_visible_in_issues() {
        let mut s = InputState::new();
        s.feed_bytes(b"teh cat");
        assert!(
            s.issues().iter().any(|i| i.word.eq_ignore_ascii_case("teh")),
            "expected `teh` in issues: {:?}",
            s.issues(),
        );
    }

    #[test]
    fn deferred_feed_plus_refresh_matches_per_byte_feed() {
        // The paste fast-path feeds the whole burst without linting, then
        // calls refresh() once. The resulting issues must match what the
        // per-byte path produces — same final buffer, same lints.
        let text = b"i beleive teh fox jumpd";

        let mut per_byte = InputState::new();
        for &b in text {
            per_byte.feed_bytes(&[b]);
        }

        let mut deferred = InputState::new();
        deferred.feed_bytes_deferred(text);
        // Before refresh, the lint cache is intentionally stale (empty).
        assert!(
            deferred.issues().is_empty(),
            "deferred feed must not run harper before refresh",
        );
        deferred.refresh();

        let mut a: Vec<&str> = per_byte.issues().iter().map(|i| i.word.as_str()).collect();
        let mut b: Vec<&str> = deferred.issues().iter().map(|i| i.word.as_str()).collect();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b, "deferred+refresh lints must match per-byte lints");
        assert_eq!(per_byte.buffer().text(), deferred.buffer().text());
    }

    #[test]
    fn refresh_is_noop_when_buffer_unchanged() {
        // Calling refresh() twice in a row must not re-run harper or
        // change the issue set — the version guard makes the second call
        // a no-op.
        let mut s = InputState::new();
        s.feed_bytes_deferred(b"teh cat");
        s.refresh();
        let first: Vec<String> = s.issues().iter().map(|i| i.word.clone()).collect();
        s.refresh();
        let second: Vec<String> = s.issues().iter().map(|i| i.word.clone()).collect();
        assert_eq!(first, second);
    }

    #[test]
    fn ascii_keystroke_sequence_yields_lints() {
        let mut s = InputState::new();
        // Simulate typing "wirte a paragprah" key-by-key
        for &b in b"wirte a paragprah" {
            s.feed_bytes(&[b]);
        }
        let words: Vec<&str> = s.issues().iter().map(|i| i.word.as_str()).collect();
        // We don't pin to the exact wording (harper may suggest different
        // corrections for different words), but at minimum it should have
        // flagged something that looks like a misspelling.
        assert!(
            !words.is_empty(),
            "expected at least one misspelling for `wirte a paragprah`, got none",
        );
    }
}
