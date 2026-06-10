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
