//! Echo tracker. Two responsibilities:
//!
//! 1. **Legacy FIFO pairing** (`pending` / `positions`). Kept for tests and
//!    possible future use; not load-bearing for paint.
//! 2. **Screen grid** (`grid` / `cell_seq`). A `(row, col) → (char, seq)`
//!    map that mirrors the current state of every screen cell the child
//!    has written to. The painter calls [`Self::find_input_anchor`] to
//!    locate the latest row that contains the buffer text contiguously,
//!    and we paint there.
//!
//! ## Why a grid, not an event-stream ring
//!
//! An earlier version searched a flat ring of recent `PrintEvent`s for a
//! contiguous run matching the buffer text. That fails against hosts that
//! interleave chrome writes (banners, footers, status) between input
//! cells inside a single frame — even though the input visually sits as a
//! contiguous row on screen, the *byte order* of the child's writes
//! breaks the run. Claude Code does exactly this.
//!
//! The grid sidesteps the problem entirely: we don't care about byte
//! order, we care about *what's currently in each cell*. The latest write
//! to each cell wins (`cell_seq` records when). Search becomes "for each
//! row, scan its cells in column order for the buffer text; pick the row
//! whose match has the largest seq."
//!
//! ## Boundaries
//!
//! `Boundary` (Enter / Esc / Ctrl-C / Ctrl-D) clears the grid and seq
//! tracking. The previous prompt's cells are no longer interesting, and
//! keeping them would risk false-matching the new input against stale
//! rows.
//!
//! ## Limitations
//! - The grid stays in absolute (row, col) terms — we don't model
//!   scrolling. Inputs that trigger scroll could leave stale entries; the
//!   `GRID_CAP` LRU eviction keeps memory bounded but matches can be
//!   wrong briefly until cells are rewritten or `Boundary` clears.
//! - Wrapped input (character-wrap, word-wrap, or host-inserted
//!   newlines) is matched by sequential advance with row-jump on
//!   gap: when a non-space target char hits an empty grid cell, the
//!   search continues on the next row at `base_col`. Continuation
//!   rows must start at `base_col` (Claude Code / Ink model).
//!   Standard terminal wrap-to-col-0 won't match.
//! - Each cell is one character, so wide CJK / emoji that the screen
//!   observer reports as 1-cell-advance will work but won't be perfect.

use std::collections::HashMap;
use std::collections::VecDeque;

use crate::event::{InputEvent, PrintEvent, ScreenEvent};
use crate::screen::CursorPos;
use crate::spell::SpellIssue;

/// Maximum number of cells we retain in the grid. Enough for a generously
/// large terminal (e.g. 400 cols × 100 rows = 40k cells exceeds this, but
/// realistic active-render footprints sit well under). When the cap is
/// exceeded we evict by oldest seq.
const GRID_CAP: usize = 16384;

pub struct EchoMatcher {
    /// Typed chars awaiting their echo, in order. Kept for compatibility
    /// with the picker plumbing; not load-bearing for paint.
    pending: VecDeque<(char, usize)>,
    /// char_offset → screen position of that char (once matched). Not used
    /// by the painter any more (see anchor search) but retained for tests
    /// and possible future use.
    positions: HashMap<usize, CursorPos>,
    /// Latest lint snapshot from the stdin thread.
    lints: Vec<SpellIssue>,
    /// Char count of the current buffer text.
    buffer_chars: usize,
    /// Current buffer text. The painter searches for this in `grid` to
    /// anchor annotations to the actual on-screen rendering.
    buffer_text: String,
    /// Char offset of the user's cursor within the buffer. Used by the
    /// picker for hover detection — "cursor inside a lint span" requires
    /// knowing where in the buffer the cursor is.
    buffer_cursor: usize,
    /// Screen cells observed via PrintEvents: (row, col) → most recent char.
    grid: HashMap<(u16, u16), char>,
    /// Per-cell update sequence number, used to pick the latest match when
    /// the buffer text appears in multiple places on screen.
    cell_seq: HashMap<(u16, u16), u64>,
    /// Monotonic counter for `cell_seq`. Wraps in theory, but at 1 µs per
    /// print event that would take ~292,000 years — practically infinite.
    next_seq: u64,
    /// Set to true on `Boundary`; cleared on the next `UserChar`. While
    /// true, the matcher is in "trailing" mode — it still accepts late echoes
    /// from the just-submitted input, but the moment a new user char arrives
    /// it counts as a fresh session and we drop the stale state.
    boundary_pending: bool,
}

impl EchoMatcher {
    pub fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            positions: HashMap::new(),
            lints: Vec::new(),
            buffer_chars: 0,
            buffer_text: String::new(),
            buffer_cursor: 0,
            grid: HashMap::new(),
            cell_seq: HashMap::new(),
            next_seq: 1,
            boundary_pending: false,
        }
    }

    /// Current buffer character count (set via the latest `Lints` event).
    pub fn buffer_chars(&self) -> usize {
        self.buffer_chars
    }

    /// Current buffer text (set via the latest `Lints` event). Read by
    /// [`Self::find_input_anchor`] internally; exposed for tests/debug.
    #[allow(dead_code)]
    pub fn buffer_text(&self) -> &str {
        &self.buffer_text
    }

    /// Apply an `InputEvent` from the stdin thread.
    ///
    /// Boundary semantics: clears the lint snapshot AND the screen grid
    /// (UI no longer cares about lints from the previous prompt; the grid
    /// would otherwise carry stale rows that could anchor-match the new
    /// input incorrectly). `pending` and `positions` survive Boundary so
    /// any trailing echoes from the just-submitted input can still match.
    /// The next `UserChar` resets those structures.
    pub fn apply_input(&mut self, ev: InputEvent) {
        match ev {
            InputEvent::UserChar { ch, char_offset } => {
                if self.boundary_pending {
                    self.pending.clear();
                    self.positions.clear();
                    self.boundary_pending = false;
                }
                self.pending.push_back((ch, char_offset));
            }
            InputEvent::Boundary => {
                self.lints.clear();
                self.buffer_chars = 0;
                self.buffer_text.clear();
                self.buffer_cursor = 0;
                self.grid.clear();
                self.cell_seq.clear();
                self.boundary_pending = true;
            }
            InputEvent::Lints {
                issues,
                buffer_chars,
                buffer_text,
                buffer_cursor,
            } => {
                self.lints = issues;
                self.buffer_chars = buffer_chars;
                self.buffer_text = buffer_text;
                self.buffer_cursor = buffer_cursor;
            }
            // Picker events are handled by the render loop directly; the
            // matcher doesn't need them.
            InputEvent::PickerState(_) => {}
        }
    }

    /// Apply a `ScreenEvent` (print or erase). Updates the screen-grid
    /// model — and, for prints, the legacy FIFO pairing.
    pub fn apply_screen_event(&mut self, ev: &ScreenEvent) {
        match ev {
            ScreenEvent::Print(p) => self.apply_print(p),
            ScreenEvent::Erase(e) => self.erase_cells(e.row, e.col_start, e.col_end),
        }
    }

    /// Apply a `PrintEvent` from the screen observer. Updates the legacy
    /// FIFO pairing and the screen-grid model. The latter is the one the
    /// painter actually reads.
    pub fn apply_print(&mut self, ev: &PrintEvent) {
        // Legacy pairing — first echo wins.
        if let Some(&(expected_ch, char_offset)) = self.pending.front()
            && expected_ch == ev.ch
        {
            self.positions.insert(char_offset, ev.at);
            self.pending.pop_front();
        }
        // Grid — each cell is overwritten by its latest char + seq.
        let key = (ev.at.row, ev.at.col);
        self.grid.insert(key, ev.ch);
        self.cell_seq.insert(key, self.next_seq);
        self.next_seq = self.next_seq.wrapping_add(1).max(1);
        if self.grid.len() > GRID_CAP {
            self.prune_grid();
        }
    }

    /// Drop the oldest quarter of grid cells. Cheap amortised across the
    /// many writes that fill the cap.
    fn prune_grid(&mut self) {
        let target_remove = self.grid.len() / 4;
        if target_remove == 0 {
            return;
        }
        let mut by_seq: Vec<((u16, u16), u64)> = self
            .cell_seq
            .iter()
            .map(|(&k, &v)| (k, v))
            .collect();
        by_seq.sort_by_key(|&(_, seq)| seq);
        for (key, _) in by_seq.iter().take(target_remove) {
            self.grid.remove(key);
            self.cell_seq.remove(key);
        }
    }

    /// Look up where the char at `char_offset` was drawn on screen. Kept
    /// for tests / debug; the painter no longer uses this map directly.
    #[allow(dead_code)]
    pub fn position_of(&self, char_offset: usize) -> Option<CursorPos> {
        self.positions.get(&char_offset).copied()
    }

    pub fn lints(&self) -> &[SpellIssue] {
        &self.lints
    }

    /// The latest screen position where the last non-empty line of
    /// buffer text is rendered contiguously on a single row. Returns
    /// the position of the *first* character of the match. None if
    /// every line of the buffer is empty (buffer is empty or all `\n`)
    /// or the text isn't currently visible on screen.
    ///
    /// ## Multi-line buffers (bracketed paste / chunk-paste)
    ///
    /// When the buffer contains `\n` (after a marker-driven paste or
    /// a chunk-shape-detected paste, see `buffer::feed_normal`), this
    /// method anchors on the last NON-EMPTY line — usually the line
    /// the cursor is on, but it falls back gracefully when the buffer
    /// ends with `\n` (paste content ending in newline) by stepping
    /// back to the previous non-empty line. The painter then uses
    /// [`Self::anchor_line_char_offset`] and the `\n` positions in
    /// `buffer_text` to compute screen rows for each earlier line,
    /// so multi-line paste content paints on every visible row.
    pub fn find_input_anchor(&self, cols: u16) -> Option<CursorPos> {
        let anchor_line = self
            .buffer_text
            .split('\n')
            .rev()
            .find(|line| !line.is_empty())?;
        find_text_in_grid(&self.grid, &self.cell_seq, anchor_line, cols)
    }

    /// Search the grid for arbitrary text and return its start position.
    /// Same matching logic as [`Self::find_input_anchor`] but for any
    /// string — used by the painter to locate earlier buffer lines in
    /// multi-line buffers.
    pub fn find_text_anchor(&self, text: &str, cols: u16) -> Option<CursorPos> {
        find_text_in_grid(&self.grid, &self.cell_seq, text, cols)
    }

    /// Character offset in `buffer_text` where the anchor line begins
    /// (i.e. the line returned by [`Self::find_input_anchor`] — the
    /// last non-empty line, which may NOT be the trailing line if the
    /// buffer ends with `\n`). Returns `0` for single-line buffers
    /// (no `\n`); for multi-line buffers, returns the char index where
    /// the chosen line starts. Picker and `fix::build_fix` use this to
    /// convert global lint offsets into anchor-line-local offsets when
    /// the user is interacting with that line. The painter does its
    /// own per-line offset math because it paints earlier lines too —
    /// see `paint::compute_new_spans`.
    pub fn anchor_line_char_offset(&self) -> usize {
        let mut last_non_empty_start = 0usize;
        let mut offset = 0usize;
        for line in self.buffer_text.split('\n') {
            let chars = line.chars().count();
            if chars > 0 {
                last_non_empty_start = offset;
            }
            offset += chars + 1;
        }
        last_non_empty_start
    }

    /// Deprecated alias for [`Self::anchor_line_char_offset`]. The
    /// "last_line" name was accurate when the painter restricted itself
    /// to the trailing line only; now that it paints every visible
    /// buffer line, "anchor_line" is the precise name. Kept as a
    /// pass-through so existing callers (picker, fix) compile until
    /// they migrate.
    pub fn last_line_char_offset(&self) -> usize {
        self.anchor_line_char_offset()
    }

    /// Look up the most-recently-printed char at a specific (row, col).
    /// None if no print event has touched that cell (or it's been evicted /
    /// erased). The painter uses this to recover the original text it
    /// previously underlined, so it can rewrite those cells with the
    /// underline attribute turned off when the corresponding lint goes
    /// away (e.g. user fixed the typo in place).
    pub fn cell_at(&self, row: u16, col: u16) -> Option<char> {
        self.grid.get(&(row, col)).copied()
    }

    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.pending.clear();
        self.positions.clear();
        self.lints.clear();
        self.buffer_chars = 0;
        self.buffer_text.clear();
        self.buffer_cursor = 0;
        self.grid.clear();
        self.cell_seq.clear();
        self.next_seq = 1;
        self.boundary_pending = false;
    }

    /// Current buffer cursor (char offset). Used by the picker's hover
    /// gate to distinguish "user is still typing" (cursor at end of
    /// buffer) from "user moved cursor into a finished word".
    pub fn buffer_cursor(&self) -> usize {
        self.buffer_cursor
    }

    /// Index into `lints()` of the lint whose char span the buffer
    /// cursor is currently inside (or just past, i.e. `cursor == char_end`).
    /// None when the cursor isn't on any actionable flagged span. Used
    /// by the picker for hover detection. Picks the rightmost match when
    /// multiple lints' spans overlap the cursor — "the one closest to
    /// where I'm typing" matches the user's intent.
    ///
    /// `grammar_enabled` is forwarded to
    /// [`crate::spell::is_actionable_category`] so Grammar lints only
    /// surface when the user opted in; without it the hover would pop on
    /// grammar issues even with the feature off.
    pub fn lint_at_cursor(&self, grammar_enabled: bool) -> Option<usize> {
        let c = self.buffer_cursor;
        let mut best: Option<(usize, usize)> = None; // (idx, char_start)
        for (i, lint) in self.lints.iter().enumerate() {
            if !crate::spell::is_actionable_category(lint.category, grammar_enabled) {
                continue;
            }
            if c >= lint.char_start && c <= lint.char_end {
                match best {
                    None => best = Some((i, lint.char_start)),
                    Some((_, prev_start)) if lint.char_start > prev_start => {
                        best = Some((i, lint.char_start));
                    }
                    _ => {}
                }
            }
        }
        best.map(|(i, _)| i)
    }

    /// Drop the screen-grid state without touching lint snapshots or the
    /// buffer text. Used when the terminal resizes (old grid positions
    /// are now potentially out-of-bounds or stale) and when the child
    /// emits erase sequences. Lints stay so the next paint can still
    /// fire as soon as the new render fills the grid.
    pub fn clear_screen(&mut self) {
        self.grid.clear();
        self.cell_seq.clear();
        self.next_seq = 1;
    }

    /// Erase a span of cells from the grid (row, [col_start, col_end)).
    /// No-op for cells that weren't tracked. Used by the screen observer
    /// when it sees CSI J / CSI K.
    pub fn erase_cells(&mut self, row: u16, col_start: u16, col_end: u16) {
        for col in col_start..col_end {
            self.grid.remove(&(row, col));
            self.cell_seq.remove(&(row, col));
        }
    }

    #[allow(dead_code)]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    #[allow(dead_code)]
    pub fn known_positions(&self) -> usize {
        self.positions.len()
    }

    #[allow(dead_code)]
    pub fn grid_len(&self) -> usize {
        self.grid.len()
    }
}

/// Search the grid for the most recent row containing `text` starting at
/// some column. "Most recent" = the candidate whose touched cells have
/// the largest max seq. Returns the start position (row, col of the first
/// char) or `None`.
///
/// ## Why this isn't a sliding-window-over-sorted-cells
///
/// We can't require the matched cells to be at consecutive grid entries:
/// Claude Code (and other Ink-based TUIs) render the user's input by
/// moving the cursor between words instead of printing a space character
/// there. So the grid has `'w','r','i','t','e'` at cols X..X+4 and
/// `'t','e','h'` at cols X+6..X+8 with **no entry** at col X+5. A naive
/// sliding-window match would fail at the space and never find the input.
///
/// Instead we iterate grid entries that match `target[0]`, and for each
/// candidate verify the rest of `target` against the grid at consecutive
/// columns — **tolerating a missing cell when the corresponding target
/// char is an ASCII space**, since a missing cell visually IS a space in
/// the terminal model. We also treat U+00A0 NBSP as equivalent to ASCII
/// space because Claude Code uses NBSP between the prompt marker and the
/// input.
///
/// Cost is O(grid_size × buffer_len) — trivial for realistic terminals.
///
/// ## Cross-row matching (soft-wrap, word-wrap, host newline)
///
/// The search advances through columns sequentially. Two kinds of row
/// transition are recognised:
///
/// 1. **Right-edge wrap** (`col >= cols`): the cursor reached the
///    terminal's right margin, so text continues on the next row at
///    `base_col`. This is the character-level wrap produced by
///    terminals and some TUI hosts.
///
/// 2. **Word-wrap / host-newline gap**: a non-space target character
///    finds an empty grid cell (`None`) before reaching the right
///    edge. This means the host wrapped text early — at a word
///    boundary (Ink / React TUIs) or at a host-inserted newline
///    (Shift-Enter in Claude Code). The search jumps to the next
///    row at `base_col` and retries. A wrong-character cell (`Some`
///    with a non-matching char) is always a genuine mismatch.
///
/// Both row transitions expect continuation at `base_col` (how
/// Claude Code / Ink TUIs render wrapped input). Standard terminal
/// wrap-to-col-0 won't match; this is consistent with the painter's
/// row-math assumption.
fn find_text_in_grid(
    grid: &HashMap<(u16, u16), char>,
    cell_seq: &HashMap<(u16, u16), u64>,
    text: &str,
    cols: u16,
) -> Option<CursorPos> {
    if text.is_empty() || grid.is_empty() || cols == 0 {
        return None;
    }
    let target: Vec<char> = text.chars().collect();
    let mut best: Option<(CursorPos, u64)> = None;

    let cell_ok = |r: u16, c: u16, ch: char| -> bool {
        match grid.get(&(r, c)) {
            Some(&gc) => chars_equiv(gc, ch),
            None => is_space_char(ch),
        }
    };

    for (&(start_row, base_col), &ch) in grid {
        if !chars_equiv(ch, target[0]) {
            continue;
        }

        let mut all_match = true;
        let mut max_seq = cell_seq
            .get(&(start_row, base_col))
            .copied()
            .unwrap_or(0);
        let mut row = start_row;
        let mut col = base_col + 1;
        let mut row_start = base_col;

        for &target_ch in target.iter().skip(1) {
            let mut just_wrapped = false;
            if col >= cols {
                row = match row.checked_add(1) {
                    Some(r) => r,
                    None => {
                        all_match = false;
                        break;
                    }
                };
                col = row_start;
                just_wrapped = true;
            }

            if cell_ok(row, col, target_ch) {
                max_seq =
                    max_seq.max(cell_seq.get(&(row, col)).copied().unwrap_or(0));
                col += 1;
            } else if just_wrapped && row_start > 0 && cell_ok(row, 0, target_ch) {
                row_start = 0;
                max_seq =
                    max_seq.max(cell_seq.get(&(row, 0)).copied().unwrap_or(0));
                col = 1;
            } else if grid.get(&(row, col)).is_none() {
                let next_row = match row.checked_add(1) {
                    Some(r) => r,
                    None => {
                        all_match = false;
                        break;
                    }
                };
                if cell_ok(next_row, base_col, target_ch) {
                    row = next_row;
                    row_start = base_col;
                    max_seq = max_seq.max(
                        cell_seq.get(&(next_row, base_col)).copied().unwrap_or(0),
                    );
                    col = base_col + 1;
                } else if base_col > 0 && cell_ok(next_row, 0, target_ch) {
                    row = next_row;
                    row_start = 0;
                    max_seq = max_seq
                        .max(cell_seq.get(&(next_row, 0)).copied().unwrap_or(0));
                    col = 1;
                } else {
                    all_match = false;
                    break;
                }
            } else {
                all_match = false;
                break;
            }
        }

        if all_match {
            let candidate = CursorPos {
                row: start_row,
                col: base_col,
            };
            match best {
                None => best = Some((candidate, max_seq)),
                Some((_, s)) if max_seq > s => best = Some((candidate, max_seq)),
                _ => {}
            }
        }
    }
    best.map(|(p, _)| p)
}

fn is_space_char(ch: char) -> bool {
    ch == ' ' || ch == '\u{a0}'
}

/// Whitespace-tolerant character equality for anchor matching. Treats
/// ASCII space and U+00A0 NBSP as interchangeable because Claude Code
/// uses NBSP for the gap right after its `❯` prompt marker, while the
/// buffer has a regular ASCII space.
fn chars_equiv(grid_ch: char, target_ch: char) -> bool {
    if grid_ch == target_ch {
        return true;
    }
    let is_space = |c: char| c == ' ' || c == '\u{a0}';
    is_space(grid_ch) && is_space(target_ch)
}

impl Default for EchoMatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(row: u16, col: u16) -> CursorPos {
        CursorPos { row, col }
    }

    #[test]
    fn empty_matcher_returns_no_positions() {
        let m = EchoMatcher::new();
        assert_eq!(m.position_of(0), None);
        assert!(m.lints().is_empty());
    }

    #[test]
    fn paired_user_then_print_records_position() {
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::UserChar { ch: 'h', char_offset: 0 });
        m.apply_print(&PrintEvent { ch: 'h', at: pos(0, 5) });
        assert_eq!(m.position_of(0), Some(pos(0, 5)));
        assert_eq!(m.pending_len(), 0);
    }

    #[test]
    fn multiple_chars_pair_in_order() {
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::UserChar { ch: 'h', char_offset: 0 });
        m.apply_input(InputEvent::UserChar { ch: 'i', char_offset: 1 });
        m.apply_print(&PrintEvent { ch: 'h', at: pos(0, 5) });
        m.apply_print(&PrintEvent { ch: 'i', at: pos(0, 6) });
        assert_eq!(m.position_of(0), Some(pos(0, 5)));
        assert_eq!(m.position_of(1), Some(pos(0, 6)));
    }

    #[test]
    fn print_with_no_pending_is_ignored() {
        let mut m = EchoMatcher::new();
        m.apply_print(&PrintEvent { ch: 'x', at: pos(0, 0) });
        // No user char was waiting — this is chrome, not echo. Ignored.
        assert_eq!(m.known_positions(), 0);
    }

    #[test]
    fn chrome_print_between_user_chars_is_skipped_not_mismatched() {
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::UserChar { ch: 'h', char_offset: 0 });
        // Chrome character — does not match 'h', should leave pending alone.
        m.apply_print(&PrintEvent { ch: '>', at: pos(0, 0) });
        assert_eq!(m.pending_len(), 1, "mismatched print should not drain pending");
        m.apply_print(&PrintEvent { ch: 'h', at: pos(0, 2) });
        assert_eq!(m.position_of(0), Some(pos(0, 2)));
    }

    #[test]
    fn boundary_preserves_state_for_trailing_echoes() {
        // Buffered scenarios (e.g. line-buffered cat) deliver all input
        // events before any echoes. If Boundary wiped pending, the trailing
        // echoes would have nothing to match. Boundary stays "soft" — it
        // marks the end of input but keeps pending/positions alive.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::UserChar { ch: 'h', char_offset: 0 });
        m.apply_print(&PrintEvent { ch: 'h', at: pos(0, 5) });
        m.apply_input(InputEvent::Boundary);
        assert_eq!(
            m.position_of(0),
            Some(pos(0, 5)),
            "position survives Boundary so trailing echoes can finish"
        );
    }

    #[test]
    fn next_user_char_after_boundary_clears_old_session() {
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::UserChar { ch: 'h', char_offset: 0 });
        m.apply_print(&PrintEvent { ch: 'h', at: pos(0, 5) });
        m.apply_input(InputEvent::Boundary);
        // New session begins:
        m.apply_input(InputEvent::UserChar { ch: 'a', char_offset: 0 });
        assert_eq!(m.position_of(0), None, "old session cleared on new input");
        assert_eq!(m.pending_len(), 1);
    }

    #[test]
    fn explicit_reset_clears_everything() {
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::UserChar { ch: 'h', char_offset: 0 });
        m.apply_print(&PrintEvent { ch: 'h', at: pos(0, 5) });
        m.reset();
        assert_eq!(m.position_of(0), None);
        assert_eq!(m.pending_len(), 0);
    }

    #[test]
    fn pipe_test_scenario_all_inputs_then_all_prints() {
        // Reproduces the bug found when piping text through `cat`: all
        // UserChars + Boundary arrive at the matcher *before* any prints.
        let mut m = EchoMatcher::new();
        for (i, ch) in "hi teh".chars().enumerate() {
            m.apply_input(InputEvent::UserChar { ch, char_offset: i });
        }
        m.apply_input(InputEvent::Boundary);
        // Now the echoes finally arrive:
        for (i, ch) in "hi teh".chars().enumerate() {
            m.apply_print(&PrintEvent { ch, at: pos(0, i as u16) });
        }
        // All six positions should have been mapped despite the Boundary in
        // between.
        for i in 0..6 {
            assert!(m.position_of(i).is_some(), "missing position for offset {i}");
        }
    }

    #[test]
    fn lints_event_stores_snapshot() {
        let mut m = EchoMatcher::new();
        let issues = vec![SpellIssue {
            byte_start: 0,
            byte_end: 3,
            char_start: 0,
            char_end: 3,
            word: "teh".into(),
            message: "spelling".into(),
            suggestions: vec!["the".into()],
            category: crate::spell::IssueCategory::Spelling,
            priority: 50,
        }];
        m.apply_input(InputEvent::Lints { issues: issues.clone(), buffer_chars: 3, buffer_text: "teh".into(), buffer_cursor: 0 });
        assert_eq!(m.lints().len(), 1);
        assert_eq!(m.lints()[0].word, "teh");
        assert_eq!(m.buffer_chars(), 3);
    }

    #[test]
    fn boundary_clears_lints_immediately() {
        let mut m = EchoMatcher::new();
        let issues = vec![SpellIssue {
            byte_start: 0,
            byte_end: 3,
            char_start: 0,
            char_end: 3,
            word: "teh".into(),
            message: "spelling".into(),
            suggestions: vec!["the".into()],
            category: crate::spell::IssueCategory::Spelling,
            priority: 50,
        }];
        m.apply_input(InputEvent::Lints { issues, buffer_chars: 3, buffer_text: "teh".into(), buffer_cursor: 0 });
        m.apply_input(InputEvent::Boundary);
        assert!(m.lints().is_empty(), "lints clear on Boundary (UI ignores old prompt)");
    }

    #[test]
    fn typing_faster_than_echo_keeps_pending() {
        let mut m = EchoMatcher::new();
        for (i, ch) in "hello".chars().enumerate() {
            m.apply_input(InputEvent::UserChar { ch, char_offset: i });
        }
        assert_eq!(m.pending_len(), 5);
        // Now the child catches up:
        for (i, ch) in "hello".chars().enumerate() {
            m.apply_print(&PrintEvent { ch, at: pos(0, i as u16) });
        }
        assert_eq!(m.pending_len(), 0);
        assert_eq!(m.position_of(2), Some(pos(0, 2)));
    }

    fn feed_prints(m: &mut EchoMatcher, row: u16, start_col: u16, text: &str) {
        for (i, ch) in text.chars().enumerate() {
            m.apply_print(&PrintEvent {
                ch,
                at: pos(row, start_col + i as u16),
            });
        }
    }

    #[test]
    fn anchor_finds_buffer_text_on_input_row() {
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 5,
            buffer_text: "hello".into(),
            buffer_cursor: 0,
        });
        feed_prints(&mut m, 7, 12, "hello");
        assert_eq!(m.find_input_anchor(80), Some(pos(7, 12)));
    }

    #[test]
    fn anchor_prefers_latest_occurrence() {
        // Same text rendered twice (e.g., preview + input). The later one
        // is the input row — the one the user is reading.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 3,
            buffer_text: "teh".into(),
            buffer_cursor: 0,
        });
        feed_prints(&mut m, 2, 4, "teh"); // earlier (preview)
        feed_prints(&mut m, 9, 6, "teh"); // later (actual input)
        assert_eq!(m.find_input_anchor(80), Some(pos(9, 6)));
    }

    #[test]
    fn anchor_rejects_non_contiguous_match() {
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 3,
            buffer_text: "abc".into(),
            buffer_cursor: 0,
        });
        // 'a' at col 5, then a gap, then 'b' at col 7, 'c' at col 8.
        m.apply_print(&PrintEvent { ch: 'a', at: pos(3, 5) });
        m.apply_print(&PrintEvent { ch: 'b', at: pos(3, 7) });
        m.apply_print(&PrintEvent { ch: 'c', at: pos(3, 8) });
        assert_eq!(m.find_input_anchor(80), None);
    }

    #[test]
    fn anchor_matches_terminal_wrap_to_col_zero() {
        // Standard terminal wrapping: text starts at col 79 (after a
        // prompt), fills to col 79, and the next character wraps to
        // col 0 of the next row. The col-0 fallback handles this.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 3,
            buffer_text: "abc".into(),
            buffer_cursor: 0,
        });
        m.apply_print(&PrintEvent { ch: 'a', at: pos(3, 79) });
        m.apply_print(&PrintEvent { ch: 'b', at: pos(4, 0) });
        m.apply_print(&PrintEvent { ch: 'c', at: pos(4, 1) });
        assert_eq!(
            m.find_input_anchor(80),
            Some(pos(3, 79)),
            "terminal wrap-to-col-0 must match via col-0 fallback"
        );
    }

    #[test]
    fn anchor_with_prompt_chrome() {
        // Claude-style prompt: "> hello" rendered on row 5 starting at col 2.
        // Anchor for "hello" lands at the 'h' position (col 4).
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 5,
            buffer_text: "hello".into(),
            buffer_cursor: 0,
        });
        feed_prints(&mut m, 5, 2, "> hello");
        assert_eq!(m.find_input_anchor(80), Some(pos(5, 4)));
    }

    #[test]
    fn anchor_empty_buffer_returns_none() {
        let m = EchoMatcher::new();
        assert_eq!(m.find_input_anchor(80), None);
    }

    #[test]
    fn anchor_text_missing_from_history_returns_none() {
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 5,
            buffer_text: "hello".into(),
            buffer_cursor: 0,
        });
        feed_prints(&mut m, 0, 0, "world");
        assert_eq!(m.find_input_anchor(80), None);
    }

    #[test]
    fn boundary_clears_grid() {
        let mut m = EchoMatcher::new();
        feed_prints(&mut m, 0, 0, "anything");
        assert!(m.grid_len() > 0);
        m.apply_input(InputEvent::Boundary);
        assert_eq!(m.grid_len(), 0);
    }

    #[test]
    fn grid_size_is_bounded() {
        let mut m = EchoMatcher::new();
        // Push many distinct cells; with the LRU eviction the grid must
        // stay at or under GRID_CAP.
        let total = GRID_CAP * 2;
        for i in 0..total {
            let row = (i / 200) as u16;
            let col = (i % 200) as u16;
            m.apply_print(&PrintEvent {
                ch: 'x',
                at: pos(row, col),
            });
        }
        assert!(m.grid_len() <= GRID_CAP, "grid grew past cap: {}", m.grid_len());
    }

    #[test]
    fn anchor_works_when_chrome_writes_interleave_with_input_cells() {
        // Simulates Claude Code's render pattern: between input cells the
        // child writes other rows (chrome, banner, status). The byte order
        // looks like W, [chrome chars on row 1], r, [chrome], i, ...
        // The grid model doesn't care about byte order — it just looks at
        // current cell state per row.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 5,
            buffer_text: "Write".into(),
            buffer_cursor: 0,
        });
        m.apply_print(&PrintEvent { ch: 'W', at: pos(5, 2) });
        feed_prints(&mut m, 1, 0, "banner row chunk");
        m.apply_print(&PrintEvent { ch: 'r', at: pos(5, 3) });
        feed_prints(&mut m, 8, 0, "footer row chunk");
        m.apply_print(&PrintEvent { ch: 'i', at: pos(5, 4) });
        m.apply_print(&PrintEvent { ch: 't', at: pos(5, 5) });
        m.apply_print(&PrintEvent { ch: 'e', at: pos(5, 6) });
        assert_eq!(m.find_input_anchor(80), Some(pos(5, 2)));
    }

    #[test]
    fn anchor_uses_latest_match_when_cell_overwritten() {
        // Buffer "abc". An old "abc" was at (3, 5), then those cells got
        // overwritten with "xyz". A new "abc" appears at (7, 10). Anchor
        // must pick (7, 10).
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 3,
            buffer_text: "abc".into(),
            buffer_cursor: 0,
        });
        feed_prints(&mut m, 3, 5, "abc");
        feed_prints(&mut m, 3, 5, "xyz"); // overwrite
        feed_prints(&mut m, 7, 10, "abc");
        assert_eq!(m.find_input_anchor(80), Some(pos(7, 10)));
    }

    #[test]
    fn clear_screen_drops_grid_but_keeps_lints() {
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![SpellIssue {
                byte_start: 0,
                byte_end: 3,
                char_start: 0,
                char_end: 3,
                word: "teh".into(),
                message: "x".into(),
                suggestions: vec!["the".into()],
                category: crate::spell::IssueCategory::Spelling,
                priority: 50,
            }],
            buffer_chars: 3,
            buffer_text: "teh".into(),
            buffer_cursor: 0,
        });
        feed_prints(&mut m, 5, 10, "teh");
        m.clear_screen();
        assert_eq!(m.grid_len(), 0, "grid should be empty after clear_screen");
        assert_eq!(m.lints().len(), 1, "lints survive clear_screen");
        assert_eq!(m.buffer_text(), "teh", "buffer text survives clear_screen");
        // No grid → no anchor → safe (painter will paint nothing).
        assert_eq!(m.find_input_anchor(80), None);
    }

    #[test]
    fn erase_cells_removes_only_requested_range() {
        let mut m = EchoMatcher::new();
        feed_prints(&mut m, 4, 10, "abcdef");
        // Erase cols 12..15 → "ab" at 10..12 and "ef" at 15..16 remain.
        m.erase_cells(4, 12, 15);
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 2,
            buffer_text: "ab".into(),
            buffer_cursor: 0,
        });
        assert_eq!(m.find_input_anchor(80), Some(pos(4, 10)));
        // Erased cells should not anchor.
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 3,
            buffer_text: "cde".into(),
            buffer_cursor: 0,
        });
        assert_eq!(m.find_input_anchor(80), None, "erased range must not anchor");
    }

    #[test]
    fn resize_then_redraw_reanchors_correctly() {
        // Simulate the full sequence the render loop sees on a resize:
        // 1. User typing fills the grid at the OLD position.
        // 2. Resize event clears the grid.
        // 3. Child's redraw refills at the NEW position.
        // 4. find_input_anchor must point at the NEW position.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 3,
            buffer_text: "teh".into(),
            buffer_cursor: 0,
        });
        // Old render: "teh" at (row=20, col=4) — typical pre-resize position.
        feed_prints(&mut m, 20, 4, "teh");
        assert_eq!(m.find_input_anchor(80), Some(pos(20, 4)));

        // Resize handler clears the grid. (This is exactly what
        // `RenderEvent::Resize` does in the render loop.)
        m.clear_screen();
        assert_eq!(m.find_input_anchor(80), None, "no anchor while grid empty");

        // Child's post-resize redraw: "teh" lands at a NEW position
        // because the layout reflowed for the new size.
        feed_prints(&mut m, 12, 6, "teh");
        assert_eq!(
            m.find_input_anchor(80),
            Some(pos(12, 6)),
            "must reanchor at new render position after resize",
        );
    }

    #[test]
    fn ed_full_screen_via_apply_screen_event_clears_grid() {
        // Mirrors what the render loop does when the child emits `\x1b[2J`
        // (CSI J mode 2): the observer emits one EraseEvent per row, and
        // the matcher drops the cells. After processing, any grid-anchor
        // search must come up empty until the child re-prints.
        let mut m = EchoMatcher::new();
        feed_prints(&mut m, 5, 0, "hello world");
        assert!(m.grid_len() > 0);
        // 10-row screen — observer would emit 10 erase events.
        for row in 0..10u16 {
            m.apply_screen_event(&ScreenEvent::Erase(crate::event::EraseEvent {
                row,
                col_start: 0,
                col_end: 80,
            }));
        }
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 5,
            buffer_text: "hello".into(),
            buffer_cursor: 0,
        });
        assert_eq!(m.find_input_anchor(80), None, "ED 2 must wipe the grid");
    }

    #[test]
    fn anchor_tolerates_cursor_skipped_space_cells() {
        // Reproduces Claude Code's render pattern: spaces between words
        // are not printed — Claude moves the cursor over them. The grid
        // has letters at letter-cols and nothing at space-cols.
        // The matcher must still anchor the buffer "write teh" at the
        // 'w' position.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 9,
            buffer_text: "write teh".into(),
            buffer_cursor: 0,
        });
        // Place only the letter cells; col 9 (the space) is intentionally
        // skipped — no PrintEvent.
        for &(col, ch) in &[
            (4u16, 'w'), (5, 'r'), (6, 'i'), (7, 't'), (8, 'e'),
            (10, 't'), (11, 'e'), (12, 'h'),
        ] {
            m.apply_print(&PrintEvent {
                ch,
                at: pos(5, col),
            });
        }
        assert_eq!(m.find_input_anchor(80), Some(pos(5, 4)));
    }

    #[test]
    fn anchor_treats_nbsp_as_equivalent_to_ascii_space() {
        // Claude Code uses U+00A0 NBSP for the gap right after the `❯`
        // prompt marker. The buffer has regular ASCII space. The matcher
        // must accept the equivalence.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 5,
            buffer_text: "a b c".into(),
            buffer_cursor: 0,
        });
        for &(col, ch) in &[
            (4u16, 'a'),
            (5, '\u{a0}'),
            (6, 'b'),
            (7, '\u{a0}'),
            (8, 'c'),
        ] {
            m.apply_print(&PrintEvent { ch, at: pos(3, col) });
        }
        assert_eq!(m.find_input_anchor(80), Some(pos(3, 4)));
    }

    #[test]
    fn anchor_still_matches_when_spaces_are_printed_explicitly() {
        // Hosts that DO print spaces (most shells) must still work — the
        // matcher accepts both cursor-skip and explicit-space rendering.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 9,
            buffer_text: "write teh".into(),
            buffer_cursor: 0,
        });
        feed_prints(&mut m, 5, 4, "write teh"); // all 9 chars including the space
        assert_eq!(m.find_input_anchor(80), Some(pos(5, 4)));
    }

    #[test]
    fn apply_screen_event_dispatches_to_print_and_erase() {
        let mut m = EchoMatcher::new();
        for (i, ch) in "hello".chars().enumerate() {
            m.apply_screen_event(&ScreenEvent::Print(PrintEvent {
                ch,
                at: pos(0, i as u16),
            }));
        }
        assert_eq!(m.grid_len(), 5);
        m.apply_screen_event(&ScreenEvent::Erase(crate::event::EraseEvent {
            row: 0,
            col_start: 1,
            col_end: 4,
        }));
        assert_eq!(m.grid_len(), 2, "erase should drop 3 cells");
    }

    #[test]
    fn last_line_char_offset_is_zero_for_single_line_buffer() {
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 5,
            buffer_text: "hello".into(),
            buffer_cursor: 5,
        });
        assert_eq!(m.last_line_char_offset(), 0);
    }

    #[test]
    fn last_line_char_offset_points_after_last_newline() {
        // Buffer "abc\ndef\nghi": last \n is byte 7, char 7. last_line
        // starts at char 8 ("ghi"). The offset is exactly that.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 11,
            buffer_text: "abc\ndef\nghi".into(),
            buffer_cursor: 11,
        });
        assert_eq!(m.last_line_char_offset(), 8);
    }

    #[test]
    fn find_input_anchor_picks_last_line_for_multi_line_buffer() {
        // Buffer "abc\ndef" — anchor must search for "def" only,
        // not the full multi-line text (which would never match
        // a single grid row).
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 7,
            buffer_text: "abc\ndef".into(),
            buffer_cursor: 7,
        });
        // "abc" rendered on row 3 starting at col 0; "def" on row 4
        // starting at col 0 (the host's view of the multi-line input).
        feed_prints(&mut m, 3, 0, "abc");
        feed_prints(&mut m, 4, 0, "def");
        assert_eq!(
            m.find_input_anchor(80),
            Some(pos(4, 0)),
            "anchor must point at the last line, not the first"
        );
    }

    #[test]
    fn find_input_anchor_falls_back_to_last_non_empty_line() {
        // Buffer "abc\n": trailing line is empty. The anchor falls
        // back to the previous non-empty line ("abc") so that any
        // lints in `abc` can still be painted — important for the
        // paste-content underline case where a pasted block may end
        // with a newline and the user's cursor sits on an empty line.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 4,
            buffer_text: "abc\n".into(),
            buffer_cursor: 4,
        });
        feed_prints(&mut m, 3, 0, "abc");
        assert_eq!(m.find_input_anchor(80), Some(pos(3, 0)));
    }

    #[test]
    fn find_input_anchor_returns_none_when_all_lines_empty() {
        // Truly empty buffer (or all `\n`) has no non-empty line to
        // anchor on. Painter does nothing in this case.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 3,
            buffer_text: "\n\n\n".into(),
            buffer_cursor: 3,
        });
        assert_eq!(m.find_input_anchor(80), None);
    }

    #[test]
    fn anchor_matches_char_wrapped_input_across_rows() {
        // Buffer text fills a 20-col row completely (character-level
        // wrap). The right-edge-wrap path fires when col reaches cols.
        let mut m = EchoMatcher::new();
        let text = "abcdefghijklmnopqrstuvwxyz";
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: text.chars().count(),
            buffer_text: text.into(),
            buffer_cursor: text.chars().count(),
        });
        // 20-col screen, text starts at col 2 → 18 chars per row.
        feed_prints(&mut m, 5, 2, "abcdefghijklmnopqr");
        feed_prints(&mut m, 6, 2, "stuvwxyz");

        assert_eq!(
            m.find_input_anchor(20),
            Some(pos(5, 2)),
            "character-wrapped text across rows must anchor correctly"
        );
    }

    #[test]
    fn anchor_matches_word_wrapped_input_across_rows() {
        // Word-wrap: the host wraps at a word boundary, leaving the
        // first row SHORT (not filling to cols). This is the common
        // case for Ink / React TUIs like Claude Code.
        // Buffer "hello world foo" on a 16-col screen starting at col 2.
        // With word wrap, "hello world" fits (11 chars) but "foo" goes
        // to the next row, leaving cols 13-15 empty on the first row.
        let mut m = EchoMatcher::new();
        let text = "hello world foo";
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: text.chars().count(),
            buffer_text: text.into(),
            buffer_cursor: text.chars().count(),
        });
        // Row 5: "hello world" at cols 2..12 (space at 7 cursor-skipped)
        // Row 6: "foo" at cols 2..4
        for &(row, col, ch) in &[
            (5u16, 2u16, 'h'), (5, 3, 'e'), (5, 4, 'l'), (5, 5, 'l'), (5, 6, 'o'),
            (5, 8, 'w'), (5, 9, 'o'), (5, 10, 'r'), (5, 11, 'l'), (5, 12, 'd'),
            (6, 2, 'f'), (6, 3, 'o'), (6, 4, 'o'),
        ] {
            m.apply_print(&PrintEvent { ch, at: pos(row, col) });
        }
        assert_eq!(
            m.find_input_anchor(16),
            Some(pos(5, 2)),
            "word-wrapped text with gap at row end must anchor correctly"
        );
    }

    #[test]
    fn anchor_cross_row_rejects_arbitrary_continuation_column() {
        // Continuation at a column that's neither base_col nor 0.
        // The search only tries these two; anything else must NOT match.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 3,
            buffer_text: "abc".into(),
            buffer_cursor: 3,
        });
        // 'a' at col 10, gap, 'b','c' at col 5,6 of next row —
        // neither base_col (10) nor col 0.
        m.apply_print(&PrintEvent { ch: 'a', at: pos(3, 10) });
        m.apply_print(&PrintEvent { ch: 'b', at: pos(4, 5) });
        m.apply_print(&PrintEvent { ch: 'c', at: pos(4, 6) });
        assert_eq!(
            m.find_input_anchor(80),
            None,
            "continuation at arbitrary column must not match"
        );
    }

    #[test]
    fn anchor_cross_row_rejects_wrong_char_not_gap() {
        // A WRONG character (Some with different char) is a genuine
        // mismatch — don't jump rows on that, only on missing cells.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 3,
            buffer_text: "abc".into(),
            buffer_cursor: 3,
        });
        // 'a' at (3, 5), 'X' at (3, 6) — wrong char, not a gap.
        // Even if 'b' exists on the next row, we must NOT jump.
        m.apply_print(&PrintEvent { ch: 'a', at: pos(3, 5) });
        m.apply_print(&PrintEvent { ch: 'X', at: pos(3, 6) });
        m.apply_print(&PrintEvent { ch: 'b', at: pos(4, 5) });
        m.apply_print(&PrintEvent { ch: 'c', at: pos(4, 6) });
        assert_eq!(
            m.find_input_anchor(80),
            None,
            "wrong char at current position must not trigger row jump"
        );
    }

    #[test]
    fn anchor_cross_row_with_spaces_tolerates_missing_cells() {
        // Soft-wrapped text with cursor-skipped spaces spanning 3 rows.
        let mut m = EchoMatcher::new();
        let text = "hello world foo bar baz qux";
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: text.chars().count(),
            buffer_text: text.into(),
            buffer_cursor: text.chars().count(),
        });
        // 14-col screen, text at col 2. Spaces are cursor-skipped.
        for &(row, col, ch) in &[
            (5u16, 2u16, 'h'), (5, 3, 'e'), (5, 4, 'l'), (5, 5, 'l'), (5, 6, 'o'),
            (5, 8, 'w'), (5, 9, 'o'), (5, 10, 'r'), (5, 11, 'l'), (5, 12, 'd'),
            (6, 2, 'f'), (6, 3, 'o'), (6, 4, 'o'),
            (6, 6, 'b'), (6, 7, 'a'), (6, 8, 'r'),
            (6, 10, 'b'), (6, 11, 'a'), (6, 12, 'z'),
            (7, 2, 'q'), (7, 3, 'u'), (7, 4, 'x'),
        ] {
            m.apply_print(&PrintEvent { ch, at: pos(row, col) });
        }
        assert_eq!(
            m.find_input_anchor(14),
            Some(pos(5, 2)),
            "cross-row search must tolerate cursor-skipped spaces"
        );
    }

    #[test]
    fn anchor_host_newline_shorter_first_row() {
        // Host inserts a newline (Shift-Enter in Claude Code) after
        // "hello." — the first row ends at col 8, well before the
        // terminal's right edge (cols=80). The buffer doesn't have
        // a \n (the keypress was an unknown CSI, ignored by tuipo).
        // "world" continues on the next row at base_col.
        let mut m = EchoMatcher::new();
        let text = "hello.world";
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: text.chars().count(),
            buffer_text: text.into(),
            buffer_cursor: text.chars().count(),
        });
        feed_prints(&mut m, 5, 2, "hello.");
        feed_prints(&mut m, 6, 2, "world");
        assert_eq!(
            m.find_input_anchor(80),
            Some(pos(5, 2)),
            "host-newline with short first row must anchor correctly"
        );
    }

    #[test]
    fn anchor_line_char_offset_points_at_last_non_empty_line() {
        // Buffer "abc\ndef\n": chars 0..3 = "abc", 4..7 = "def",
        // 8 = "" (empty trailing line). Anchor falls back to "def"
        // which starts at char 4.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 8,
            buffer_text: "abc\ndef\n".into(),
            buffer_cursor: 8,
        });
        assert_eq!(m.anchor_line_char_offset(), 4);
    }
}
