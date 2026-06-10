//! Passive screen-state observer.
//!
//! Sits on the PTY→stdout output stream and parses ANSI sequences with `vte`
//! to track where the child is drawing. Phase 3 only — does NOT modify
//! output, does NOT render anything. Just observes.
//!
//! Why this matters: later phases need to know "where on the screen has the
//! user's text been echoed?" so we can paint annotations there (phase 5) and
//! dock the suggestion picker near the offending word (phase 6). We don't
//! model a full screen grid — just cursor position, alt-screen mode, and the
//! saved-cursor stack. That's the minimum that makes echo-tracking work.

use vte::{Params, Parser, Perform};

use crate::event::{EraseEvent, PrintEvent, ScreenEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPos {
    pub row: u16,
    pub col: u16,
}

#[derive(Debug, Clone)]
pub struct ScreenState {
    pub cursor: CursorPos,
    pub saved_cursor: Option<CursorPos>,
    pub cols: u16,
    pub rows: u16,
    pub alt_screen: bool,
    /// xterm's deferred-wrap flag. After printing in the last column, the
    /// cursor stays at the last column but `pending_wrap` is set; the next
    /// print causes the wrap before placing the character. Any explicit
    /// cursor move (CR, LF, CUP, etc.) clears this.
    pub pending_wrap: bool,
    /// Increments on every state change. Lets consumers cheaply detect
    /// "did anything I care about happen?"
    pub version: u64,
}

impl ScreenState {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cursor: CursorPos { row: 0, col: 0 },
            saved_cursor: None,
            cols: cols.max(1),
            rows: rows.max(1),
            alt_screen: false,
            pending_wrap: false,
            version: 0,
        }
    }

    /// Update on SIGWINCH. Clamps the cursor if the new size is smaller.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        if self.cursor.row >= self.rows {
            self.cursor.row = self.rows - 1;
        }
        if self.cursor.col >= self.cols {
            self.cursor.col = self.cols - 1;
        }
        self.bump_version();
    }

    fn bump_version(&mut self) {
        self.version = self.version.wrapping_add(1);
    }
}

pub struct ScreenObserver {
    parser: Parser,
    state: ScreenState,
}

impl ScreenObserver {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            parser: Parser::new(),
            state: ScreenState::new(cols, rows),
        }
    }

    /// Feed bytes that the child has just written. Cheap; doesn't allocate.
    /// Use this when you don't need the per-char print event stream.
    #[allow(dead_code)]
    pub fn observe(&mut self, bytes: &[u8]) {
        let mut performer = StatePerformer {
            state: &mut self.state,
            events: None,
        };
        self.parser.advance(&mut performer, bytes);
    }

    /// Like [`observe`], but also pushes a `ScreenEvent` (print or erase)
    /// for every cell-level change the child produced. Print events
    /// carry the (char, position); erase events carry the (row, col
    /// range). The matcher consumes both in order so the grid model
    /// stays in sync with the visible screen.
    pub fn observe_with_events(&mut self, bytes: &[u8], events: &mut Vec<ScreenEvent>) {
        let mut performer = StatePerformer {
            state: &mut self.state,
            events: Some(events),
        };
        self.parser.advance(&mut performer, bytes);
    }

    /// Used by phase 4 (echo-tracking) and the debug log.
    #[allow(dead_code)]
    pub fn state(&self) -> &ScreenState {
        &self.state
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.state.resize(cols, rows);
    }
}

/// Internal performer that mutates `ScreenState`. Lifetime-bound to the
/// observer so we don't have to clone state for each `advance` call.
struct StatePerformer<'a> {
    state: &'a mut ScreenState,
    /// Optional sink for cell-level events. Filled by `observe_with_events`.
    events: Option<&'a mut Vec<ScreenEvent>>,
}

impl<'a> StatePerformer<'a> {
    fn push_print(&mut self, ch: char, at: crate::screen::CursorPos) {
        if let Some(events) = self.events.as_deref_mut() {
            events.push(ScreenEvent::Print(PrintEvent { ch, at }));
        }
    }

    fn push_erase(&mut self, row: u16, col_start: u16, col_end: u16) {
        if col_end <= col_start {
            return;
        }
        if let Some(events) = self.events.as_deref_mut() {
            events.push(ScreenEvent::Erase(EraseEvent { row, col_start, col_end }));
        }
    }

    /// Emit erase events for CSI J modes.
    fn emit_ed(&mut self, mode: u16) {
        let cols = self.state.cols;
        let rows = self.state.rows;
        let cur_row = self.state.cursor.row;
        let cur_col = self.state.cursor.col;
        match mode {
            // 0 = cursor to end of screen.
            0 => {
                self.push_erase(cur_row, cur_col, cols);
                for r in (cur_row + 1)..rows {
                    self.push_erase(r, 0, cols);
                }
            }
            // 1 = start of screen to cursor (inclusive of cursor cell).
            1 => {
                for r in 0..cur_row {
                    self.push_erase(r, 0, cols);
                }
                self.push_erase(cur_row, 0, cur_col.saturating_add(1).min(cols));
            }
            // 2 or 3 = entire screen.
            _ => {
                for r in 0..rows {
                    self.push_erase(r, 0, cols);
                }
            }
        }
    }

    /// Emit erase events for CSI K modes.
    fn emit_el(&mut self, mode: u16) {
        let cols = self.state.cols;
        let row = self.state.cursor.row;
        let cur_col = self.state.cursor.col;
        match mode {
            0 => self.push_erase(row, cur_col, cols),
            1 => self.push_erase(row, 0, cur_col.saturating_add(1).min(cols)),
            _ => self.push_erase(row, 0, cols),
        }
    }
}

impl Perform for StatePerformer<'_> {
    fn print(&mut self, c: char) {
        // Deferred wrap: if the previous print left us pending at the right
        // edge, do the wrap now BEFORE placing this character, then advance.
        // Double-width chars (CJK / emoji) aren't modeled — phase-7 polish.
        let last_col = self.state.cols.saturating_sub(1);
        if self.state.pending_wrap {
            if self.state.cursor.row + 1 < self.state.rows {
                self.state.cursor.row += 1;
            }
            self.state.cursor.col = 0;
            self.state.pending_wrap = false;
        }
        // Emit the print event with the cursor pos BEFORE we advance — that's
        // the cell where the character is visible.
        let at = self.state.cursor;
        self.push_print(c, at);
        if self.state.cursor.col < last_col {
            self.state.cursor.col += 1;
        } else {
            // We're at the last column; the char is drawn here, cursor stays,
            // but next print will wrap.
            self.state.pending_wrap = true;
        }
        self.state.bump_version();
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => {
                if self.state.cursor.col > 0 {
                    self.state.cursor.col -= 1;
                    self.state.pending_wrap = false;
                    self.state.bump_version();
                }
            }
            0x09 => {
                let last_col = self.state.cols.saturating_sub(1);
                let next = (self.state.cursor.col / 8 + 1) * 8;
                self.state.cursor.col = next.min(last_col);
                self.state.pending_wrap = false;
                self.state.bump_version();
            }
            0x0A..=0x0C => {
                if self.state.cursor.row + 1 < self.state.rows {
                    self.state.cursor.row += 1;
                    self.state.pending_wrap = false;
                    self.state.bump_version();
                }
            }
            0x0D => {
                if self.state.cursor.col != 0 || self.state.pending_wrap {
                    self.state.cursor.col = 0;
                    self.state.pending_wrap = false;
                    self.state.bump_version();
                }
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let private = intermediates.first().copied();
        let p1 = first_param_or(params, 1);
        let p2 = second_param_or(params, 1);
        let last_col = self.state.cols.saturating_sub(1);
        let last_row = self.state.rows.saturating_sub(1);

        match action {
            'A' => {
                self.state.cursor.row = self.state.cursor.row.saturating_sub(p1);
                self.state.bump_version();
            }
            'B' => {
                self.state.cursor.row = (self.state.cursor.row + p1).min(last_row);
                self.state.bump_version();
            }
            'C' => {
                self.state.cursor.col = (self.state.cursor.col + p1).min(last_col);
                self.state.bump_version();
            }
            'D' => {
                self.state.cursor.col = self.state.cursor.col.saturating_sub(p1);
                self.state.bump_version();
            }
            'E' => {
                self.state.cursor.row = (self.state.cursor.row + p1).min(last_row);
                self.state.cursor.col = 0;
                self.state.bump_version();
            }
            'F' => {
                self.state.cursor.row = self.state.cursor.row.saturating_sub(p1);
                self.state.cursor.col = 0;
                self.state.bump_version();
            }
            'G' => {
                self.state.cursor.col = p1.saturating_sub(1).min(last_col);
                self.state.bump_version();
            }
            'd' => {
                self.state.cursor.row = p1.saturating_sub(1).min(last_row);
                self.state.bump_version();
            }
            'H' | 'f' => {
                self.state.cursor.row = p1.saturating_sub(1).min(last_row);
                self.state.cursor.col = p2.saturating_sub(1).min(last_col);
                self.state.pending_wrap = false;
                self.state.bump_version();
            }
            's' if private.is_none() => {
                self.state.saved_cursor = Some(self.state.cursor);
            }
            'u' if private.is_none() => {
                if let Some(c) = self.state.saved_cursor {
                    self.state.cursor = c;
                    self.state.pending_wrap = false;
                    self.state.bump_version();
                }
            }
            'h' => {
                if private == Some(b'?') && is_alt_screen_mode(params) {
                    self.state.alt_screen = true;
                    self.state.bump_version();
                }
            }
            'l' => {
                if private == Some(b'?') && is_alt_screen_mode(params) {
                    self.state.alt_screen = false;
                    self.state.bump_version();
                }
            }
            // ED: erase display. Param 0 (default) = cursor to end of
            // screen; 1 = start of screen to cursor; 2 = entire screen;
            // 3 = entire screen + scrollback (we treat same as 2). The
            // cursor doesn't move, but cells are erased — we emit one
            // `EraseEvent` per affected row so the matcher drops stale
            // grid entries.
            'J' => {
                let mode = params
                    .iter()
                    .next()
                    .and_then(|p| p.first().copied())
                    .unwrap_or(0);
                self.emit_ed(mode);
            }
            // EL: erase line. Param 0 = cursor to end of line; 1 = start
            // of line to cursor; 2 = entire line.
            'K' => {
                let mode = params
                    .iter()
                    .next()
                    .and_then(|p| p.first().copied())
                    .unwrap_or(0);
                self.emit_el(mode);
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'7' => {
                self.state.saved_cursor = Some(self.state.cursor);
            }
            b'8' => {
                if let Some(c) = self.state.saved_cursor {
                    self.state.cursor = c;
                    self.state.pending_wrap = false;
                    self.state.bump_version();
                }
            }
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}
}

fn first_param_or(params: &Params, default: u16) -> u16 {
    params
        .iter()
        .next()
        .and_then(|p| p.first().copied())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

fn second_param_or(params: &Params, default: u16) -> u16 {
    params
        .iter()
        .nth(1)
        .and_then(|p| p.first().copied())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

/// Recognise the three flavors of alt-screen mode (legacy 47, smcup 1047,
/// and DEC private 1049 which also saves the cursor).
fn is_alt_screen_mode(params: &Params) -> bool {
    for group in params.iter() {
        if let Some(&n) = group.first()
            && matches!(n, 47 | 1047 | 1049)
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs() -> ScreenObserver {
        ScreenObserver::new(80, 24)
    }

    fn feed(o: &mut ScreenObserver, bytes: &[u8]) {
        o.observe(bytes);
    }

    fn feed_events(o: &mut ScreenObserver, bytes: &[u8]) -> Vec<ScreenEvent> {
        let mut ev = Vec::new();
        o.observe_with_events(bytes, &mut ev);
        ev
    }

    fn prints(events: &[ScreenEvent]) -> Vec<PrintEvent> {
        events
            .iter()
            .filter_map(|e| match e {
                ScreenEvent::Print(p) => Some(*p),
                ScreenEvent::Erase(_) => None,
            })
            .collect()
    }

    fn erases(events: &[ScreenEvent]) -> Vec<EraseEvent> {
        events
            .iter()
            .filter_map(|e| match e {
                ScreenEvent::Erase(er) => Some(*er),
                ScreenEvent::Print(_) => None,
            })
            .collect()
    }

    #[test]
    fn initial_state_is_origin() {
        let o = obs();
        assert_eq!(o.state().cursor, CursorPos { row: 0, col: 0 });
        assert!(!o.state().alt_screen);
    }

    #[test]
    fn printing_advances_cursor_column() {
        let mut o = obs();
        feed(&mut o, b"hi");
        assert_eq!(o.state().cursor, CursorPos { row: 0, col: 2 });
    }

    #[test]
    fn cr_returns_to_column_zero() {
        let mut o = obs();
        feed(&mut o, b"hi\r");
        assert_eq!(o.state().cursor, CursorPos { row: 0, col: 0 });
    }

    #[test]
    fn lf_advances_row_keeping_column() {
        let mut o = obs();
        feed(&mut o, b"hi\n");
        // LF in raw-mode terminals does NOT reset col (no implicit CR).
        assert_eq!(o.state().cursor, CursorPos { row: 1, col: 2 });
    }

    #[test]
    fn crlf_goes_to_next_line_col_zero() {
        let mut o = obs();
        feed(&mut o, b"hi\r\n");
        assert_eq!(o.state().cursor, CursorPos { row: 1, col: 0 });
    }

    #[test]
    fn backspace_moves_cursor_left() {
        let mut o = obs();
        feed(&mut o, b"abc\x08");
        assert_eq!(o.state().cursor, CursorPos { row: 0, col: 2 });
    }

    #[test]
    fn backspace_at_col_zero_is_clamped() {
        let mut o = obs();
        feed(&mut o, b"\x08");
        assert_eq!(o.state().cursor, CursorPos { row: 0, col: 0 });
    }

    #[test]
    fn cup_sets_cursor_position_1_indexed() {
        let mut o = obs();
        feed(&mut o, b"\x1b[5;10H");
        assert_eq!(o.state().cursor, CursorPos { row: 4, col: 9 });
    }

    #[test]
    fn cup_with_no_params_goes_to_origin() {
        let mut o = obs();
        feed(&mut o, b"\x1b[10;10H\x1b[H");
        assert_eq!(o.state().cursor, CursorPos { row: 0, col: 0 });
    }

    #[test]
    fn cup_clamps_to_screen_bounds() {
        let mut o = ScreenObserver::new(10, 5);
        feed(&mut o, b"\x1b[100;100H");
        assert_eq!(o.state().cursor, CursorPos { row: 4, col: 9 });
    }

    #[test]
    fn cursor_up_csi_a() {
        let mut o = obs();
        feed(&mut o, b"\x1b[5;5H\x1b[2A");
        assert_eq!(o.state().cursor.row, 2);
        assert_eq!(o.state().cursor.col, 4);
    }

    #[test]
    fn cursor_down_csi_b() {
        let mut o = obs();
        feed(&mut o, b"\x1b[3B");
        assert_eq!(o.state().cursor.row, 3);
    }

    #[test]
    fn cursor_forward_csi_c() {
        let mut o = obs();
        feed(&mut o, b"\x1b[7C");
        assert_eq!(o.state().cursor.col, 7);
    }

    #[test]
    fn cursor_back_csi_d() {
        let mut o = obs();
        feed(&mut o, b"\x1b[10C\x1b[3D");
        assert_eq!(o.state().cursor.col, 7);
    }

    #[test]
    fn cursor_horizontal_absolute_csi_g() {
        let mut o = obs();
        feed(&mut o, b"\x1b[5;5H\x1b[20G");
        assert_eq!(o.state().cursor, CursorPos { row: 4, col: 19 });
    }

    #[test]
    fn cursor_vertical_absolute_csi_d_lowercase() {
        let mut o = obs();
        feed(&mut o, b"\x1b[5;5H\x1b[10d");
        assert_eq!(o.state().cursor, CursorPos { row: 9, col: 4 });
    }

    #[test]
    fn save_and_restore_via_csi_su() {
        let mut o = obs();
        feed(&mut o, b"\x1b[3;7H\x1b[s\x1b[10;10H\x1b[u");
        assert_eq!(o.state().cursor, CursorPos { row: 2, col: 6 });
    }

    #[test]
    fn save_and_restore_via_esc_7_8() {
        let mut o = obs();
        feed(&mut o, b"\x1b[3;7H\x1b7\x1b[10;10H\x1b8");
        assert_eq!(o.state().cursor, CursorPos { row: 2, col: 6 });
    }

    #[test]
    fn alt_screen_1049_toggles() {
        let mut o = obs();
        assert!(!o.state().alt_screen);
        feed(&mut o, b"\x1b[?1049h");
        assert!(o.state().alt_screen);
        feed(&mut o, b"\x1b[?1049l");
        assert!(!o.state().alt_screen);
    }

    #[test]
    fn alt_screen_47_and_1047_recognised() {
        let mut o = obs();
        feed(&mut o, b"\x1b[?47h");
        assert!(o.state().alt_screen);
        feed(&mut o, b"\x1b[?47l");
        assert!(!o.state().alt_screen);
        feed(&mut o, b"\x1b[?1047h");
        assert!(o.state().alt_screen);
    }

    #[test]
    fn non_alt_private_modes_dont_toggle_alt_flag() {
        let mut o = obs();
        // ?25 is cursor visibility, ?7 is autowrap, ?2004 is bracketed paste.
        feed(&mut o, b"\x1b[?25h\x1b[?7h\x1b[?2004h");
        assert!(!o.state().alt_screen);
    }

    #[test]
    fn erase_display_does_not_move_cursor() {
        let mut o = obs();
        feed(&mut o, b"\x1b[5;5H\x1b[2J");
        assert_eq!(o.state().cursor, CursorPos { row: 4, col: 4 });
    }

    #[test]
    fn resize_clamps_cursor() {
        let mut o = ScreenObserver::new(80, 24);
        feed(&mut o, b"\x1b[20;70H");
        o.resize(50, 10);
        assert!(o.state().cursor.row < 10);
        assert!(o.state().cursor.col < 50);
    }

    #[test]
    fn version_increments_only_on_meaningful_change() {
        let mut o = obs();
        let v0 = o.state().version;
        // Receive a benign ED — no cursor move.
        feed(&mut o, b"\x1b[2J");
        assert_eq!(o.state().version, v0, "ED shouldn't bump version");
        feed(&mut o, b"a");
        assert!(o.state().version > v0);
    }

    #[test]
    fn print_wraps_at_right_edge() {
        // xterm-style deferred wrap: after printing 5 chars on a 5-wide row,
        // cursor stays at the last column with `pending_wrap` set. The wrap
        // doesn't fire until the next print.
        let mut o = ScreenObserver::new(5, 10);
        feed(&mut o, b"abcde");
        assert_eq!(o.state().cursor, CursorPos { row: 0, col: 4 });
        assert!(o.state().pending_wrap);
        feed(&mut o, b"f");
        // 'f' is drawn at (1, 0), then cursor advances to (1, 1).
        assert_eq!(o.state().cursor, CursorPos { row: 1, col: 1 });
        assert!(!o.state().pending_wrap);
    }

    #[test]
    fn cr_clears_pending_wrap() {
        let mut o = ScreenObserver::new(5, 10);
        feed(&mut o, b"abcde");
        assert!(o.state().pending_wrap);
        feed(&mut o, b"\r");
        assert!(!o.state().pending_wrap);
        feed(&mut o, b"x");
        // x was printed at (0,0), cursor advances to (0,1).
        assert_eq!(o.state().cursor, CursorPos { row: 0, col: 1 });
    }

    #[test]
    fn typical_claude_startup_sequence_does_not_panic() {
        // Mocked-up startup: enter alt screen, clear, position cursor, draw,
        // toggle a private mode. We just verify nothing crashes and the
        // final state is sensible.
        let mut o = obs();
        feed(
            &mut o,
            b"\x1b[?1049h\x1b[2J\x1b[H\x1b[?25l\
              \x1b[1;1HClaude Code\r\n\
              \x1b[3;1H> \x1b[?25h\x1b[?2004h",
        );
        assert!(o.state().alt_screen);
    }

    #[test]
    fn ansi_color_codes_dont_disturb_position() {
        let mut o = obs();
        feed(
            &mut o,
            b"\x1b[31mred\x1b[0m \x1b[1mbold\x1b[0m \x1b[4mund\x1b[0m",
        );
        // 'red' (3) + ' ' (1) + 'bold' (4) + ' ' (1) + 'und' (3) = 12 cells
        assert_eq!(o.state().cursor.col, 12);
    }

    #[test]
    fn print_events_capture_chars_at_pre_print_positions() {
        let mut o = obs();
        let events = feed_events(&mut o, b"abc");
        let p = prints(&events);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0], PrintEvent { ch: 'a', at: CursorPos { row: 0, col: 0 } });
        assert_eq!(p[1], PrintEvent { ch: 'b', at: CursorPos { row: 0, col: 1 } });
        assert_eq!(p[2], PrintEvent { ch: 'c', at: CursorPos { row: 0, col: 2 } });
    }

    #[test]
    fn print_events_after_cup_use_new_position() {
        let mut o = obs();
        let events = feed_events(&mut o, b"\x1b[10;5Hx");
        let p = prints(&events);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0], PrintEvent { ch: 'x', at: CursorPos { row: 9, col: 4 } });
    }

    #[test]
    fn print_events_skip_control_chars_and_csi() {
        let mut o = obs();
        let events = feed_events(&mut o, b"a\r\n\x1b[31mb\x1b[0m");
        let chars: Vec<char> = prints(&events).iter().map(|e| e.ch).collect();
        assert_eq!(chars, vec!['a', 'b']);
    }

    #[test]
    fn csi_k_default_emits_erase_from_cursor_to_eol() {
        // Cursor at col 5 on row 3, 80-wide screen → erase cols [5, 80).
        let mut o = obs();
        feed(&mut o, b"\x1b[4;6H"); // move to (row 3, col 5)
        let events = feed_events(&mut o, b"\x1b[K");
        let e = erases(&events);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0], EraseEvent { row: 3, col_start: 5, col_end: 80 });
    }

    #[test]
    fn csi_k_mode_2_erases_entire_line() {
        let mut o = obs();
        feed(&mut o, b"\x1b[2;3H");
        let events = feed_events(&mut o, b"\x1b[2K");
        let e = erases(&events);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0], EraseEvent { row: 1, col_start: 0, col_end: 80 });
    }

    #[test]
    fn csi_j_mode_2_erases_entire_screen() {
        // 5-row × 10-col screen → 5 erase events, one per row.
        let mut o = ScreenObserver::new(10, 5);
        let events = feed_events(&mut o, b"\x1b[2J");
        let e = erases(&events);
        assert_eq!(e.len(), 5);
        for (row, ev) in e.iter().enumerate() {
            assert_eq!(*ev, EraseEvent { row: row as u16, col_start: 0, col_end: 10 });
        }
    }

    #[test]
    fn csi_j_default_erases_from_cursor_to_end_of_screen() {
        // 5×10 screen. Cursor at (row=2, col=3). Default ED clears:
        //   row 2, cols 3..10
        //   row 3, cols 0..10
        //   row 4, cols 0..10
        let mut o = ScreenObserver::new(10, 5);
        feed(&mut o, b"\x1b[3;4H");
        let events = feed_events(&mut o, b"\x1b[J");
        let e = erases(&events);
        assert_eq!(e.len(), 3);
        assert_eq!(e[0], EraseEvent { row: 2, col_start: 3, col_end: 10 });
        assert_eq!(e[1], EraseEvent { row: 3, col_start: 0, col_end: 10 });
        assert_eq!(e[2], EraseEvent { row: 4, col_start: 0, col_end: 10 });
    }

    #[test]
    fn osc_window_title_is_silently_consumed() {
        let mut o = obs();
        feed(&mut o, b"hello\x1b]0;Window Title\x07world");
        // OSC consumed; cursor advanced for "hello" + "world" = 10 chars.
        assert_eq!(o.state().cursor.col, 10);
    }
}
