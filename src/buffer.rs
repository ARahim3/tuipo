//! Keystroke buffer state machine.
//!
//! Sees every byte the user types and reconstructs the text of the current
//! input line. The wrapped child still owns the rendering — this is purely
//! tuipo's authoritative model of "what has the user typed so far on this
//! input." Reset on Enter, Esc-alone, Ctrl-C, Ctrl-D.
//!
//! What this *can't* know: edits that go through commands the child
//! interprets but we don't (Meta-B word-back, Ctrl-K kill-line, readline
//! history navigation, etc.). The buffer will drift in those cases. The
//! remedy is bounded scope — drift can't survive past the next Enter, and
//! later phases verify positions via screen-echo tracking before painting
//! anything.

use std::str;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedOutcome {
    /// Byte consumed but no semantically interesting change (escape-sequence
    /// internals, UTF-8 continuation in progress, ignored control char).
    NoChange,
    /// Buffer text or cursor position changed.
    Updated,
    /// Input boundary: buffer was reset (Enter, Esc-alone, Ctrl-C, Ctrl-D).
    Boundary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    Normal,
    AfterEsc,
    InCsi,
    InOsc,
    InSs3,
}

pub struct InputBuffer {
    text: String,
    /// Cursor position as a byte offset into `text`. Always on a UTF-8
    /// boundary so `text[..cursor]` is a valid string slice.
    cursor: usize,
    state: ParseState,
    csi_params: String,
    /// How many UTF-8 continuation bytes we're still waiting on.
    utf8_remaining: u8,
    utf8_partial: Vec<u8>,
    in_paste: bool,
    /// Set externally by the stdin pump when the current `read()` chunk
    /// looks like a paste burst even though no bracketed-paste markers
    /// arrived. Claude Code (and some other Ink-based TUIs) don't enable
    /// bracketed paste, so we'd otherwise see real `\n` bytes mid-stream
    /// and (wrongly) treat them as boundary submits. While this flag is
    /// true, the CR/LF→Boundary branch in `feed_normal` is suppressed —
    /// same effect as `in_paste`, just driven by a chunk-shape heuristic
    /// instead of marker bytes. See `pty::chunk_looks_like_paste`.
    chunk_paste: bool,
    /// Monotonically increasing every time `text` or `cursor` changes. Lets
    /// downstream consumers cheaply check "is the buffer in the same state
    /// as the last time I looked at it?"
    version: u64,
}

impl InputBuffer {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            state: ParseState::Normal,
            csi_params: String::new(),
            utf8_remaining: 0,
            utf8_partial: Vec::new(),
            in_paste: false,
            chunk_paste: false,
            version: 0,
        }
    }

    /// Externally-driven paste flag, set by the stdin pump when the
    /// current read chunk looks like a paste burst (see
    /// `pty::chunk_looks_like_paste`). Should be set true BEFORE the
    /// pump iterates the bytes of the chunk and false AFTER, so a `\n`
    /// in the middle of a paste-shaped chunk is inserted as content
    /// rather than triggering Boundary. The internal `in_paste` flag
    /// driven by `\x1b[200~` / `\x1b[201~` markers still works
    /// independently; they're OR'd at the boundary check.
    pub fn set_chunk_paste(&mut self, flag: bool) {
        self.chunk_paste = flag;
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Byte offset of the cursor. Used by phase 3 (screen position mapping).
    #[allow(dead_code)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// Used by phase 5 (skip live painting during paste).
    #[allow(dead_code)]
    pub fn in_paste(&self) -> bool {
        self.in_paste
    }

    /// Used by tests; will be used by phase 7 status row to hide when empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Feed a slice of bytes; returns the aggregate outcome. Boundary wins
    /// over Updated wins over NoChange (so callers can clear caches on
    /// boundary or invalidate lints on update).
    pub fn feed_bytes(&mut self, bytes: &[u8]) -> FeedOutcome {
        let mut acc = FeedOutcome::NoChange;
        for &b in bytes {
            let r = self.feed(b);
            acc = combine(acc, r);
        }
        acc
    }

    pub fn feed(&mut self, byte: u8) -> FeedOutcome {
        match self.state {
            ParseState::Normal => self.feed_normal(byte),
            ParseState::AfterEsc => self.feed_after_esc(byte),
            ParseState::InCsi => self.feed_in_csi(byte),
            ParseState::InOsc => self.feed_in_osc(byte),
            ParseState::InSs3 => self.feed_in_ss3(byte),
        }
    }

    fn feed_normal(&mut self, byte: u8) -> FeedOutcome {
        // Mid-UTF-8: every byte goes into the partial buffer until complete.
        if self.utf8_remaining > 0 {
            self.utf8_partial.push(byte);
            self.utf8_remaining -= 1;
            if self.utf8_remaining == 0 {
                let bytes = std::mem::take(&mut self.utf8_partial);
                if let Ok(s) = str::from_utf8(&bytes) {
                    self.insert_str(s);
                    return FeedOutcome::Updated;
                }
                // Invalid sequence: drop silently. Buffer stays valid.
            }
            return FeedOutcome::NoChange;
        }

        match byte {
            0x1B => {
                self.state = ParseState::AfterEsc;
                FeedOutcome::NoChange
            }
            // Backspace / DEL
            0x08 | 0x7F => self.delete_before_cursor(),
            // CR, LF — input boundary, EXCEPT during paste. Two signals
            // can flag "we're inside a paste":
            //   1. `in_paste` — set/cleared by the bracketed-paste
            //      markers `\x1b[200~` / `\x1b[201~`. Works whenever the
            //      child enabled bracketed paste mode.
            //   2. `chunk_paste` — set by the stdin pump for the
            //      duration of a paste-shaped read chunk (multiple
            //      newlines in one read, or content after a newline in
            //      one read). Fallback for hosts that DON'T enable
            //      bracketed paste — Claude Code's v2.x build doesn't
            //      send `\x1b[?2004h`, so its pastes arrive marker-less.
            // Under either signal we insert `\n` as content (canonicalised
            // from CR) so the buffer retains the full multi-line text.
            // The painter then anchors on the trailing line — see
            // `EchoMatcher::find_input_anchor`.
            0x0D | 0x0A => {
                if self.in_paste || self.chunk_paste {
                    self.insert_str("\n");
                    FeedOutcome::Updated
                } else {
                    self.boundary_reset()
                }
            }
            // Ctrl-C, Ctrl-D — input boundary
            0x03 | 0x04 => self.boundary_reset(),
            // Ctrl-A: beginning of line
            0x01 => self.set_cursor(0),
            // Ctrl-E: end of line
            0x05 => self.set_cursor(self.text.len()),
            // Ctrl-U: kill to beginning of line
            0x15 => self.kill_to_start(),
            // Ctrl-W: delete word before cursor
            0x17 => self.delete_word_before_cursor(),
            // Tab: tuipo reserves it for the suggestion picker; do not insert.
            0x09 => FeedOutcome::NoChange,
            // Other control chars — ignore
            0..=0x1F => FeedOutcome::NoChange,
            // ASCII printable
            0x20..=0x7E => {
                self.insert_str(&(byte as char).to_string());
                FeedOutcome::Updated
            }
            // UTF-8 lead bytes
            0xC0..=0xDF => {
                self.begin_utf8(byte, 1);
                FeedOutcome::NoChange
            }
            0xE0..=0xEF => {
                self.begin_utf8(byte, 2);
                FeedOutcome::NoChange
            }
            0xF0..=0xF7 => {
                self.begin_utf8(byte, 3);
                FeedOutcome::NoChange
            }
            _ => FeedOutcome::NoChange,
        }
    }

    fn feed_after_esc(&mut self, byte: u8) -> FeedOutcome {
        match byte {
            b'[' => {
                self.state = ParseState::InCsi;
                self.csi_params.clear();
                FeedOutcome::NoChange
            }
            b']' => {
                self.state = ParseState::InOsc;
                FeedOutcome::NoChange
            }
            b'O' => {
                self.state = ParseState::InSs3;
                FeedOutcome::NoChange
            }
            // Bare ESC press: treat as cancel/boundary.
            // (User pressed Esc to abandon the current input.)
            _ => {
                self.state = ParseState::Normal;
                self.boundary_reset()
            }
        }
    }

    fn feed_in_csi(&mut self, byte: u8) -> FeedOutcome {
        // CSI grammar: parameter bytes 0x30-0x3F, intermediate 0x20-0x2F,
        // final 0x40-0x7E. We don't distinguish param vs intermediate; we
        // just accumulate everything before the final.
        if (0x40..=0x7E).contains(&byte) {
            let params = std::mem::take(&mut self.csi_params);
            self.state = ParseState::Normal;
            self.handle_csi(&params, byte)
        } else if (0x20..=0x3F).contains(&byte) {
            self.csi_params.push(byte as char);
            FeedOutcome::NoChange
        } else {
            // Invalid byte inside CSI — abandon the sequence to avoid a stuck state.
            self.state = ParseState::Normal;
            FeedOutcome::NoChange
        }
    }

    fn handle_csi(&mut self, params: &str, final_byte: u8) -> FeedOutcome {
        match final_byte {
            // Up / Down: history navigation, doesn't affect our line buffer.
            b'A' | b'B' => FeedOutcome::NoChange,
            b'C' => self.move_cursor_right(),
            b'D' => self.move_cursor_left(),
            b'H' => self.set_cursor(0),
            b'F' => self.set_cursor(self.text.len()),
            b'~' => match params {
                "3" => self.delete_at_cursor(),
                "200" => {
                    self.in_paste = true;
                    FeedOutcome::NoChange
                }
                "201" => {
                    self.in_paste = false;
                    FeedOutcome::NoChange
                }
                _ => FeedOutcome::NoChange,
            },
            _ => FeedOutcome::NoChange,
        }
    }

    fn feed_in_osc(&mut self, byte: u8) -> FeedOutcome {
        // OSC ends on BEL (0x07) or ST (ESC \). We approximate ST by
        // transitioning to AfterEsc on ESC and trusting the next byte.
        if byte == 0x07 {
            self.state = ParseState::Normal;
        } else if byte == 0x1B {
            self.state = ParseState::AfterEsc;
        }
        FeedOutcome::NoChange
    }

    fn feed_in_ss3(&mut self, byte: u8) -> FeedOutcome {
        // ESC O <final>: application-mode cursor / F1–F4.
        let outcome = match byte {
            b'A' | b'B' => FeedOutcome::NoChange,
            b'C' => self.move_cursor_right(),
            b'D' => self.move_cursor_left(),
            b'H' => self.set_cursor(0),
            b'F' => self.set_cursor(self.text.len()),
            _ => FeedOutcome::NoChange,
        };
        self.state = ParseState::Normal;
        outcome
    }

    fn begin_utf8(&mut self, lead: u8, remaining: u8) {
        self.utf8_partial.clear();
        self.utf8_partial.push(lead);
        self.utf8_remaining = remaining;
    }

    fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.bump_version();
    }

    fn delete_before_cursor(&mut self) -> FeedOutcome {
        if self.cursor == 0 {
            return FeedOutcome::NoChange;
        }
        let prefix = &self.text[..self.cursor];
        let Some(c) = prefix.chars().next_back() else {
            return FeedOutcome::NoChange;
        };
        let start = self.cursor - c.len_utf8();
        self.text.drain(start..self.cursor);
        self.cursor = start;
        self.bump_version();
        FeedOutcome::Updated
    }

    fn delete_at_cursor(&mut self) -> FeedOutcome {
        if self.cursor >= self.text.len() {
            return FeedOutcome::NoChange;
        }
        let rest = &self.text[self.cursor..];
        let Some(c) = rest.chars().next() else {
            return FeedOutcome::NoChange;
        };
        let end = self.cursor + c.len_utf8();
        self.text.drain(self.cursor..end);
        self.bump_version();
        FeedOutcome::Updated
    }

    fn delete_word_before_cursor(&mut self) -> FeedOutcome {
        if self.cursor == 0 {
            return FeedOutcome::NoChange;
        }
        let prefix = &self.text[..self.cursor];
        let mut new_cursor = self.cursor;
        let mut iter = prefix.char_indices().rev().peekable();
        // Strip any trailing whitespace
        while let Some(&(idx, c)) = iter.peek() {
            if !c.is_whitespace() {
                break;
            }
            new_cursor = idx;
            iter.next();
        }
        // Then strip the word itself
        while let Some(&(idx, c)) = iter.peek() {
            if c.is_whitespace() {
                break;
            }
            new_cursor = idx;
            iter.next();
        }
        if new_cursor == self.cursor {
            return FeedOutcome::NoChange;
        }
        self.text.drain(new_cursor..self.cursor);
        self.cursor = new_cursor;
        self.bump_version();
        FeedOutcome::Updated
    }

    fn kill_to_start(&mut self) -> FeedOutcome {
        if self.cursor == 0 {
            return FeedOutcome::NoChange;
        }
        self.text.drain(..self.cursor);
        self.cursor = 0;
        self.bump_version();
        FeedOutcome::Updated
    }

    fn move_cursor_left(&mut self) -> FeedOutcome {
        if self.cursor == 0 {
            return FeedOutcome::NoChange;
        }
        let prefix = &self.text[..self.cursor];
        let Some(c) = prefix.chars().next_back() else {
            return FeedOutcome::NoChange;
        };
        self.cursor -= c.len_utf8();
        self.bump_version();
        FeedOutcome::Updated
    }

    fn move_cursor_right(&mut self) -> FeedOutcome {
        if self.cursor >= self.text.len() {
            return FeedOutcome::NoChange;
        }
        let rest = &self.text[self.cursor..];
        let Some(c) = rest.chars().next() else {
            return FeedOutcome::NoChange;
        };
        self.cursor += c.len_utf8();
        self.bump_version();
        FeedOutcome::Updated
    }

    fn set_cursor(&mut self, pos: usize) -> FeedOutcome {
        let new_pos = pos.min(self.text.len());
        if new_pos == self.cursor {
            return FeedOutcome::NoChange;
        }
        // Snap to the nearest UTF-8 boundary at or before `new_pos`.
        let mut snapped = new_pos;
        while snapped > 0 && !self.text.is_char_boundary(snapped) {
            snapped -= 1;
        }
        self.cursor = snapped;
        self.bump_version();
        FeedOutcome::Updated
    }

    fn boundary_reset(&mut self) -> FeedOutcome {
        self.text.clear();
        self.cursor = 0;
        self.utf8_remaining = 0;
        self.utf8_partial.clear();
        self.in_paste = false;
        self.csi_params.clear();
        self.state = ParseState::Normal;
        self.bump_version();
        FeedOutcome::Boundary
    }

    fn bump_version(&mut self) {
        self.version = self.version.wrapping_add(1);
    }
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

fn combine(a: FeedOutcome, b: FeedOutcome) -> FeedOutcome {
    use FeedOutcome::*;
    match (a, b) {
        (Boundary, _) | (_, Boundary) => Boundary,
        (Updated, _) | (_, Updated) => Updated,
        _ => NoChange,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf_with(input: &[u8]) -> InputBuffer {
        let mut b = InputBuffer::new();
        b.feed_bytes(input);
        b
    }

    #[test]
    fn ascii_typing_builds_text() {
        let b = buf_with(b"hello");
        assert_eq!(b.text(), "hello");
        assert_eq!(b.cursor(), 5);
    }

    #[test]
    fn backspace_removes_last_char() {
        let mut b = buf_with(b"hello");
        b.feed(0x08);
        assert_eq!(b.text(), "hell");
        assert_eq!(b.cursor(), 4);
    }

    #[test]
    fn del_byte_also_removes_last_char() {
        let mut b = buf_with(b"hi");
        b.feed(0x7F);
        assert_eq!(b.text(), "h");
        assert_eq!(b.cursor(), 1);
    }

    #[test]
    fn backspace_on_empty_is_noop() {
        let mut b = InputBuffer::new();
        let out = b.feed(0x08);
        assert_eq!(out, FeedOutcome::NoChange);
        assert_eq!(b.text(), "");
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn enter_resets_buffer() {
        let mut b = buf_with(b"hello world");
        let out = b.feed(0x0D);
        assert_eq!(out, FeedOutcome::Boundary);
        assert!(b.is_empty());
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn newline_also_resets_buffer() {
        let mut b = buf_with(b"hello world");
        let out = b.feed(0x0A);
        assert_eq!(out, FeedOutcome::Boundary);
        assert!(b.is_empty());
    }

    #[test]
    fn ctrl_c_resets_buffer() {
        let mut b = buf_with(b"partial");
        let out = b.feed(0x03);
        assert_eq!(out, FeedOutcome::Boundary);
        assert!(b.is_empty());
    }

    #[test]
    fn ctrl_d_resets_buffer() {
        let mut b = buf_with(b"partial");
        let out = b.feed(0x04);
        assert_eq!(out, FeedOutcome::Boundary);
        assert!(b.is_empty());
    }

    #[test]
    fn bare_esc_resets_buffer() {
        let mut b = buf_with(b"partial");
        let out = b.feed_bytes(&[0x1B, b'q']);
        assert_eq!(out, FeedOutcome::Boundary);
        assert!(b.is_empty());
    }

    #[test]
    fn tab_is_swallowed() {
        let mut b = buf_with(b"go");
        let out = b.feed(0x09);
        assert_eq!(out, FeedOutcome::NoChange);
        assert_eq!(b.text(), "go");
    }

    #[test]
    fn utf8_two_byte_char_assembled() {
        // 'é' = 0xC3 0xA9
        let b = buf_with(b"caf\xC3\xA9");
        assert_eq!(b.text(), "café");
        assert_eq!(b.cursor(), "café".len());
    }

    #[test]
    fn utf8_three_byte_char_assembled() {
        // '日' = 0xE6 0x97 0xA5
        let b = buf_with(b"\xE6\x97\xA5");
        assert_eq!(b.text(), "日");
    }

    #[test]
    fn utf8_four_byte_char_assembled() {
        // '🦀' = 0xF0 0x9F 0xA6 0x80
        let b = buf_with(b"\xF0\x9F\xA6\x80");
        assert_eq!(b.text(), "🦀");
    }

    #[test]
    fn partial_utf8_keeps_buffer_valid() {
        let mut b = InputBuffer::new();
        b.feed(b'\xC3'); // start of 'é', missing continuation
        assert_eq!(b.text(), "");
        // Now finish it
        b.feed(b'\xA9');
        assert_eq!(b.text(), "é");
    }

    #[test]
    fn utf8_backspace_removes_full_codepoint() {
        let mut b = buf_with("café".as_bytes());
        b.feed(0x08);
        assert_eq!(b.text(), "caf");
        assert_eq!(b.cursor(), 3);
    }

    #[test]
    fn utf8_backspace_removes_emoji() {
        let mut b = buf_with("hi 🦀".as_bytes());
        b.feed(0x08);
        assert_eq!(b.text(), "hi ");
        assert_eq!(b.cursor(), 3);
    }

    #[test]
    fn left_arrow_moves_cursor() {
        let mut b = buf_with(b"abc");
        let out = b.feed_bytes(&[0x1B, b'[', b'D']);
        assert_eq!(out, FeedOutcome::Updated);
        assert_eq!(b.cursor(), 2);
        assert_eq!(b.text(), "abc");
    }

    #[test]
    fn right_arrow_moves_cursor() {
        let mut b = buf_with(b"abc");
        b.feed_bytes(&[0x1B, b'[', b'D']);
        b.feed_bytes(&[0x1B, b'[', b'D']);
        assert_eq!(b.cursor(), 1);
        b.feed_bytes(&[0x1B, b'[', b'C']);
        assert_eq!(b.cursor(), 2);
    }

    #[test]
    fn home_jumps_cursor_to_start() {
        let mut b = buf_with(b"abc");
        b.feed_bytes(&[0x1B, b'[', b'H']);
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn end_jumps_cursor_to_end() {
        let mut b = buf_with(b"abc");
        b.feed_bytes(&[0x1B, b'[', b'H']);
        b.feed_bytes(&[0x1B, b'[', b'F']);
        assert_eq!(b.cursor(), 3);
    }

    #[test]
    fn delete_key_removes_char_at_cursor() {
        let mut b = buf_with(b"abc");
        // Cursor at end; move left, then Delete
        b.feed_bytes(&[0x1B, b'[', b'D']);
        b.feed_bytes(&[0x1B, b'[', b'3', b'~']);
        assert_eq!(b.text(), "ab");
    }

    #[test]
    fn ctrl_a_goes_to_start_ctrl_e_to_end() {
        let mut b = buf_with(b"abcdef");
        b.feed(0x01);
        assert_eq!(b.cursor(), 0);
        b.feed(0x05);
        assert_eq!(b.cursor(), 6);
    }

    #[test]
    fn ctrl_u_kills_to_start() {
        let mut b = buf_with(b"hello world");
        // cursor at end; nothing after cursor → drains everything
        b.feed(0x15);
        assert_eq!(b.text(), "");
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn ctrl_u_only_kills_before_cursor() {
        let mut b = buf_with(b"hello world");
        // move cursor left twice with left arrow
        b.feed_bytes(&[0x1B, b'[', b'D']);
        b.feed_bytes(&[0x1B, b'[', b'D']);
        // cursor at byte 9 ("hello wor|ld"). Ctrl-U kills "hello wor"
        b.feed(0x15);
        assert_eq!(b.text(), "ld");
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn ctrl_w_deletes_word_back() {
        let mut b = buf_with(b"hello world");
        b.feed(0x17);
        assert_eq!(b.text(), "hello ");
    }

    #[test]
    fn ctrl_w_strips_trailing_whitespace_first() {
        let mut b = buf_with(b"hello world  ");
        b.feed(0x17);
        assert_eq!(b.text(), "hello ");
    }

    #[test]
    fn ctrl_w_at_start_is_noop() {
        let mut b = InputBuffer::new();
        let out = b.feed(0x17);
        assert_eq!(out, FeedOutcome::NoChange);
    }

    #[test]
    fn ss3_arrows_also_move_cursor() {
        let mut b = buf_with(b"abc");
        // ESC O D (application-mode left arrow)
        b.feed_bytes(&[0x1B, b'O', b'D']);
        assert_eq!(b.cursor(), 2);
        b.feed_bytes(&[0x1B, b'O', b'C']);
        assert_eq!(b.cursor(), 3);
    }

    #[test]
    fn osc_sequence_is_silently_consumed() {
        let mut b = buf_with(b"foo");
        // ESC ] 0;title BEL
        let osc: Vec<u8> = [0x1B, b']', b'0', b';', b't', b'i', b't', b'l', b'e', 0x07].to_vec();
        b.feed_bytes(&osc);
        assert_eq!(b.text(), "foo");
        // Followed by typing should still work:
        b.feed_bytes(b"!");
        assert_eq!(b.text(), "foo!");
    }

    #[test]
    fn bracketed_paste_flag_toggles() {
        let mut b = InputBuffer::new();
        b.feed_bytes(b"x");
        assert!(!b.in_paste());
        b.feed_bytes(&[0x1B, b'[', b'2', b'0', b'0', b'~']);
        assert!(b.in_paste());
        b.feed_bytes(b"pasted text");
        assert!(b.in_paste());
        assert_eq!(b.text(), "xpasted text");
        b.feed_bytes(&[0x1B, b'[', b'2', b'0', b'1', b'~']);
        assert!(!b.in_paste());
    }

    #[test]
    fn unknown_csi_doesnt_break_subsequent_typing() {
        let mut b = buf_with(b"a");
        // Mouse-y looking CSI: ESC [ < 35;1;1 M (a mouse event)
        let mouse: Vec<u8> = [0x1B, b'[', b'<', b'3', b'5', b';', b'1', b';', b'1', b'M'].to_vec();
        b.feed_bytes(&mouse);
        b.feed_bytes(b"bc");
        assert_eq!(b.text(), "abc");
    }

    #[test]
    fn version_increments_on_change() {
        let mut b = InputBuffer::new();
        let v0 = b.version();
        b.feed(b'a');
        assert!(b.version() > v0);
        let v1 = b.version();
        b.feed_bytes(&[0x1B, b'[', b'D']); // left arrow when cursor=1; updates
        assert!(b.version() > v1);
    }

    #[test]
    fn version_does_not_change_on_noop_feed() {
        let mut b = InputBuffer::new();
        b.feed(b'a');
        let v = b.version();
        b.feed(0x09); // tab — swallowed
        assert_eq!(b.version(), v);
    }

    #[test]
    fn cursor_is_always_on_char_boundary() {
        // After many UTF-8 edits, cursor should never split a codepoint.
        let mut b = buf_with("café 🦀 日本".as_bytes());
        // Move cursor around; each step should keep the invariant.
        for _ in 0..30 {
            b.feed_bytes(&[0x1B, b'[', b'D']);
            assert!(b.text().is_char_boundary(b.cursor()));
        }
        for _ in 0..30 {
            b.feed_bytes(&[0x1B, b'[', b'C']);
            assert!(b.text().is_char_boundary(b.cursor()));
        }
    }

    #[test]
    fn typing_after_left_arrow_inserts_at_cursor() {
        let mut b = buf_with(b"helo");
        b.feed_bytes(&[0x1B, b'[', b'D']);
        b.feed_bytes(&[0x1B, b'[', b'D']);
        // cursor at "he|lo"
        b.feed(b'l');
        assert_eq!(b.text(), "hello");
    }

    #[test]
    fn input_boundary_clears_paste_flag() {
        let mut b = InputBuffer::new();
        b.feed_bytes(&[0x1B, b'[', b'2', b'0', b'0', b'~']);
        assert!(b.in_paste());
        // Outside the bracketed-paste flag-toggle context: a real Enter
        // by the user. (Inside paste, see `paste_preserves_newlines_as_content`.)
        b.feed_bytes(&[0x1B, b'[', b'2', b'0', b'1', b'~']);
        assert!(!b.in_paste());
        b.feed(0x0D);
        assert!(!b.in_paste());
    }

    #[test]
    fn paste_preserves_newlines_as_content() {
        // Multi-line bracketed paste: `\x1b[200~ ... \n ... \x1b[201~`.
        // Without the in_paste guard around the CR/LF boundary in
        // feed_normal, each pasted newline would Boundary the buffer
        // mid-paste, wiping lints + the matcher's grid and leaving
        // subsequent typing un-linted (Claude Code paste-of-prose
        // user-report).
        let mut b = InputBuffer::new();
        b.feed_bytes(&[0x1B, b'[', b'2', b'0', b'0', b'~']);
        let mut had_boundary = false;
        for &byte in b"line one\nline two\nline three".iter() {
            let out = b.feed(byte);
            if out == FeedOutcome::Boundary {
                had_boundary = true;
            }
        }
        b.feed_bytes(&[0x1B, b'[', b'2', b'0', b'1', b'~']);
        assert!(!had_boundary, "no Boundary should fire during paste");
        assert_eq!(
            b.text(),
            "line one\nline two\nline three",
            "buffer must retain pasted newlines as content"
        );
    }

    #[test]
    fn paste_canonicalises_cr_to_lf() {
        // Windows-style CRLF in paste content shouldn't double up
        // separators or surprise the painter's last-line offset
        // computation, which keys on `\n` only.
        let mut b = InputBuffer::new();
        b.feed_bytes(&[0x1B, b'[', b'2', b'0', b'0', b'~']);
        // "a\r\nb" — both CR and LF should land as a single \n.
        b.feed_bytes(b"a\r\nb");
        b.feed_bytes(&[0x1B, b'[', b'2', b'0', b'1', b'~']);
        assert_eq!(
            b.text(),
            "a\n\nb",
            "each of CR and LF inserts one \\n in paste-mode (no special CRLF folding)"
        );
    }

    #[test]
    fn newline_outside_paste_still_boundaries() {
        // The guard must only apply inside `\x1b[200~ ... \x1b[201~`.
        // Outside, Enter / Shift-Enter still need to clear the buffer
        // so the next prompt starts from a clean state.
        let mut b = buf_with(b"some text");
        let out = b.feed(0x0A);
        assert_eq!(out, FeedOutcome::Boundary);
        assert!(b.is_empty());
    }

    #[test]
    fn chunk_paste_flag_suppresses_newline_boundary() {
        // Marker-less paste path: the stdin pump's chunk-shape detector
        // calls `set_chunk_paste(true)` before iterating the bytes of a
        // paste-shaped read. Inside that window, real `\n` bytes must
        // be inserted as content — same effect as bracketed-paste
        // mode, just driven by a different signal. Without this, hosts
        // that don't enable bracketed paste (Claude Code v2.x) would
        // see every internal newline of a multi-line paste fire
        // Boundary and wipe the input buffer mid-paste.
        let mut b = InputBuffer::new();
        b.set_chunk_paste(true);
        let mut had_boundary = false;
        for &byte in b"first line\nsecond line\nthird".iter() {
            if b.feed(byte) == FeedOutcome::Boundary {
                had_boundary = true;
            }
        }
        b.set_chunk_paste(false);
        assert!(!had_boundary, "no Boundary should fire while chunk_paste is set");
        assert_eq!(
            b.text(),
            "first line\nsecond line\nthird",
            "buffer must accumulate the multi-line paste content"
        );
    }

    #[test]
    fn chunk_paste_does_not_persist_after_clearing() {
        // Once the pump clears `chunk_paste` at the end of the chunk,
        // a subsequent real Enter must still boundary. Otherwise the
        // chunk-paste flag would silently disable Enter forever.
        let mut b = InputBuffer::new();
        b.set_chunk_paste(true);
        b.feed_bytes(b"pasted\nline");
        b.set_chunk_paste(false);
        // Now user presses Enter — this is a single-byte chunk.
        let out = b.feed(b'\n');
        assert_eq!(out, FeedOutcome::Boundary);
        assert!(b.is_empty());
    }
}
