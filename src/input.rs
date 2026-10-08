//! Combines the keystroke buffer with the spell engine and caches the result.
//!
//! Owning these together lets later phases stay simple — render code asks
//! `state.issues()` and gets back the current lints without worrying about
//! re-running harper, debouncing, or version bookkeeping.
//!
//! In production the engine is the lint worker (`lint_worker.rs`): buffer
//! changes are handed to it without waiting, and [`InputState::sync_issues`]
//! brings `issues()` up to date with the live buffer before anything acts
//! on it (Tab-fix, the picker). Unit tests build the state with
//! [`InputState::with_checker`] instead, which runs harper synchronously on
//! every change so assertions can read `issues()` straight after a feed.

use std::time::Duration;

use crate::buffer::{FeedOutcome, InputBuffer};
use crate::lint_worker::LintHandle;
use crate::spell::SpellIssue;
#[cfg(test)]
use crate::spell::SpellChecker;

pub struct InputState {
    buffer: InputBuffer,
    lints: Lints,
    issues: Vec<SpellIssue>,
    last_checked_version: u64,
}

enum Lints {
    /// Asynchronous: results come back through the worker.
    Worker(LintHandle),
    /// Synchronous harper on every change — unit tests only.
    #[cfg(test)]
    Inline(Box<SpellChecker>),
}

impl InputState {
    pub fn new(lints: LintHandle) -> Self {
        Self {
            buffer: InputBuffer::new(),
            lints: Lints::Worker(lints),
            issues: Vec::new(),
            last_checked_version: 0,
        }
    }

    #[cfg(test)]
    pub fn with_checker() -> Self {
        Self {
            buffer: InputBuffer::new(),
            lints: Lints::Inline(Box::default()),
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

    /// Feed bytes to the buffer and hand any change to the lint engine.
    /// On `Boundary` with the lint worker, this also emits
    /// `InputEvent::Boundary` to the render loop — see
    /// [`LintHandle::boundary`] for why that send lives there.
    pub fn feed_bytes(&mut self, bytes: &[u8]) -> FeedOutcome {
        let outcome = self.buffer.feed_bytes(bytes);
        match outcome {
            FeedOutcome::Updated => self.buffer_changed(),
            FeedOutcome::Boundary => {
                self.issues.clear();
                self.last_checked_version = self.buffer.version();
                match &self.lints {
                    Lints::Worker(handle) => handle.boundary(),
                    #[cfg(test)]
                    Lints::Inline(_) => {}
                }
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

    /// Hand the current buffer to the lint engine. Pairs with
    /// [`Self::feed_bytes_deferred`].
    pub fn refresh(&mut self) {
        self.buffer_changed();
    }

    /// False while linting is paused (`tuipo off`): Tab-fix and the picker
    /// should stand aside and let Tab and arrows reach the child.
    pub fn lints_enabled(&self) -> bool {
        match &self.lints {
            Lints::Worker(handle) => handle.enabled(),
            #[cfg(test)]
            Lints::Inline(_) => true,
        }
    }

    /// Bring `issues()` in line with the live buffer, waiting up to
    /// `timeout` for the lint worker to publish lints for exactly this
    /// text. Returns false (and leaves no issues) if they don't arrive in
    /// time, so callers fall back to "nothing to fix" rather than acting
    /// on spans computed for different text.
    pub fn sync_issues(&mut self, timeout: Duration) -> bool {
        match &self.lints {
            Lints::Worker(handle) => {
                let fresh = handle.issues_for(self.buffer.text(), timeout);
                let synced = fresh.is_some();
                self.issues = fresh.unwrap_or_default();
                synced
            }
            #[cfg(test)]
            Lints::Inline(_) => true,
        }
    }

    fn buffer_changed(&mut self) {
        match &mut self.lints {
            Lints::Worker(handle) => {
                // Text or cursor moved. The worker re-lints only when the
                // text changed; a cursor-only move just re-emits the last
                // snapshot with the new cursor for the picker's hover gate.
                handle.update(self.buffer.text(), self.buffer.cursor_chars());
            }
            #[cfg(test)]
            Lints::Inline(checker) => {
                let v = self.buffer.version();
                if v == self.last_checked_version {
                    return;
                }
                self.last_checked_version = v;
                self.issues = checker.check(self.buffer.text());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_clears_issues() {
        let mut s = InputState::with_checker();
        s.feed_bytes(b"teh ");
        let before = s.issues().len();
        // We expect at least one lint on "teh"
        assert!(before > 0, "expected issues from `teh `, got {before}");
        s.feed_bytes(&[0x0D]);
        assert!(s.issues().is_empty(), "issues survived boundary");
    }

    #[test]
    fn issues_update_on_change() {
        let mut s = InputState::with_checker();
        s.feed_bytes(b"hello");
        let v0 = s.buffer().version();
        s.feed_bytes(b" teh");
        assert_ne!(v0, s.buffer().version());
    }

    #[test]
    fn misspelling_is_visible_in_issues() {
        let mut s = InputState::with_checker();
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

        let mut per_byte = InputState::with_checker();
        for &b in text {
            per_byte.feed_bytes(&[b]);
        }

        let mut deferred = InputState::with_checker();
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
        let mut s = InputState::with_checker();
        s.feed_bytes_deferred(b"teh cat");
        s.refresh();
        let first: Vec<String> = s.issues().iter().map(|i| i.word.clone()).collect();
        s.refresh();
        let second: Vec<String> = s.issues().iter().map(|i| i.word.clone()).collect();
        assert_eq!(first, second);
    }

    #[test]
    fn ascii_keystroke_sequence_yields_lints() {
        let mut s = InputState::with_checker();
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
