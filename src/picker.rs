//! Suggestion picker — opt-in interactive UI for choosing among harper's
//! suggestions for a misspelling.
//!
//! Enabled via `picker = true` in `~/.config/tuipo/config.toml`. Default
//! is off; the bare-minimum tuipo stays as quiet as it was before, with
//! Tab passing through and only the underline overlay surfacing as
//! feedback.
//!
//! When enabled, two interactions co-exist:
//!
//! **Hover (passive).** Move the buffer cursor inside a flagged span;
//! after `hover_ms` of idle the picker renders as a tooltip beside the
//! word. No keys are captured — the user can keep typing and dismiss
//! it just by moving the cursor out.
//!
//! **Engaged (interactive).** Pressing Tab while the cursor is in/just
//! past a flagged span engages the picker. Arrow keys navigate, Enter
//! applies the selected suggestion, Esc dismisses. Engaged-mode keys
//! are eaten by the stdin pump and never reach the wrapped child.
//!
//! ## Rendering: inline-right, fall back above
//!
//! The overlay tries to render one row to the right of the misspelled
//! word. If there isn't enough column room, it falls back to the row
//! above the anchor. If neither fits (very narrow terminal AND the lint
//! is on row 0) we skip rendering — better to be silent than to land
//! the picker in a wrong place.
//!
//! ## Painted-cells tracking + restore
//!
//! The picker writes directly to the local terminal (not through the
//! PTY), so the host doesn't know we drew there. We track the row +
//! column range we overlaid and rewrite from the screen-grid on dismiss,
//! similar to the underline stale-clear. Repaint happens every tick to
//! defeat the host's redraws (same defense underlines use, see paint.rs
//! pivot #3).
//!
//! ## Why the engaged state lives in the stdin thread
//!
//! Engaged-mode keystrokes must be intercepted *before* they reach the
//! wrapped child. The stdin pump is the only thread that owns the raw
//! byte stream coming from the user; the render thread runs downstream
//! of it. So engaged-mode state machinery (and its small escape-sequence
//! parser for arrow keys) sits in pty.rs alongside the existing stdin
//! pump. Render state (hover detection, overlay region tracking, the
//! snapshot to display) lives in this module's helpers and is updated
//! from the render loop.

use std::io::Write;
use std::time::Instant;

use crate::echo::EchoMatcher;
use crate::event::PickerSnapshot;
use crate::paint;
use crate::screen::ScreenState;
use crate::spell::SpellIssue;

/// Engaged-mode state owned by the stdin pump. None means closed; Some
/// means arrow/Enter/Esc are captured by the picker.
#[derive(Debug, Clone)]
pub struct Engaged {
    pub target_char_start: usize,
    pub target_char_end: usize,
    pub target_word: String,
    pub suggestions: Vec<String>,
    pub selected: usize,
}

impl Engaged {
    pub fn snapshot(&self) -> PickerSnapshot {
        PickerSnapshot {
            target: self.target_word.clone(),
            suggestions: self.suggestions.clone(),
            selected: self.selected,
        }
    }

    /// Move the selection by `delta` with wrap-around. `+1` = next,
    /// `-1` = previous. Wrap matches Grammarly-style keyboard nav.
    pub fn nav(&mut self, delta: isize) {
        if self.suggestions.is_empty() {
            return;
        }
        let len = self.suggestions.len() as isize;
        let next = (self.selected as isize + delta).rem_euclid(len);
        self.selected = next as usize;
    }

    pub fn selected_suggestion(&self) -> &str {
        &self.suggestions[self.selected]
    }
}

/// Build engaged state from a lint. Returns None if the issue has no
/// suggestions (we never engage on an unactionable lint).
pub fn engage_for(issue: &SpellIssue) -> Option<Engaged> {
    if issue.suggestions.is_empty() {
        return None;
    }
    Some(Engaged {
        target_char_start: issue.char_start,
        target_char_end: issue.char_end,
        target_word: issue.word.clone(),
        suggestions: issue.suggestions.clone(),
        selected: 0,
    })
}

/// Region of the local terminal that the picker is currently occupying.
/// Tracked in the render loop so we know which cells to restore from the
/// grid when the picker dismisses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayRegion {
    pub row: u16,
    pub col_start: u16,
    pub col_end: u16,
}

/// Render-thread state for the picker UI. Tracks both the explicit
/// snapshot (from the stdin pump's PickerState event when engaged) and
/// the implicit hover trigger (cursor idle inside a lint).
#[derive(Debug, Default)]
pub struct OverlayGate {
    /// Explicit engaged snapshot, sent by stdin via PickerState. When
    /// Some, takes precedence over hover.
    pub engaged_snapshot: Option<PickerSnapshot>,
    /// Where on screen we last rendered the picker. Used to restore from
    /// the grid when the picker disappears.
    pub region: Option<OverlayRegion>,
    /// Time the buffer cursor last moved. Hover fires after this idle
    /// window elapses with the cursor inside a flagged span.
    pub cursor_changed_at: Option<Instant>,
    /// Last buffer cursor we saw; used to detect "the cursor moved."
    pub last_cursor: Option<usize>,
    /// Last buffer char count we saw. Used to detect "the buffer just
    /// shrank" — backspace / Ctrl-W / delete-key — which means the user
    /// is editing a finished word rather than typing forward. That signal
    /// lets the end-of-buffer hover gate relax (see `check_hover`).
    pub last_buffer_chars: Option<usize>,
    /// True when the most recent buffer change reduced `buffer_chars`.
    /// Reset by the next Lints event whose buffer didn't shrink, and by
    /// `reset_buffer_tracking` on Boundary so a fresh prompt doesn't
    /// inherit a stale shrink flag from the previous one.
    pub buffer_shrank: bool,
    /// Which lint index the hover tooltip is currently showing for.
    /// Distinct from `engaged_snapshot` — hover never captures input.
    pub hover_lint: Option<usize>,
}

impl OverlayGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update bookkeeping when a Lints event arrives. Tracks cursor-move
    /// timestamps so the hover idle window is measured correctly AND
    /// records whether the buffer just shrank — the latter feeds the
    /// "backspaced back into a flagged word" relaxation in
    /// [`check_hover`].
    pub fn note_buffer(&mut self, cursor: usize, buffer_chars: usize, now: Instant) {
        // Buffer growth direction. None → first Lints since the gate was
        // constructed or last reset; treat as "no shrink yet."
        self.buffer_shrank = matches!(self.last_buffer_chars, Some(prev) if buffer_chars < prev);
        self.last_buffer_chars = Some(buffer_chars);
        if Some(cursor) != self.last_cursor {
            self.last_cursor = Some(cursor);
            self.cursor_changed_at = Some(now);
            // Cursor moved → any prior hover should re-evaluate.
            self.hover_lint = None;
        }
    }

    /// Drop buffer-direction tracking. Called on `Boundary` (Enter / Esc /
    /// Ctrl-C / Ctrl-D) so the next prompt starts from a clean slate.
    /// Without this, a Boundary while the buffer was long would leave
    /// `last_buffer_chars = Some(N)`; the next prompt's first keystroke
    /// arrives with `buffer_chars = 1` and would falsely register as a
    /// shrink (`1 < N`) — opening the hover gate on the very first char
    /// of a new prompt.
    pub fn reset_buffer_tracking(&mut self) {
        self.last_buffer_chars = None;
        self.buffer_shrank = false;
        self.last_cursor = None;
        self.cursor_changed_at = None;
        self.hover_lint = None;
    }

    /// Called when stdin notifies us the engaged picker changed.
    pub fn set_engaged(&mut self, snap: Option<PickerSnapshot>) {
        self.engaged_snapshot = snap;
    }

    /// Whether the picker should currently render anything.
    pub fn snapshot_to_render<'a>(&'a self, lints: &'a [SpellIssue]) -> Option<RenderTarget<'a>> {
        if let Some(snap) = &self.engaged_snapshot {
            return Some(RenderTarget::Engaged(snap));
        }
        if let Some(idx) = self.hover_lint
            && let Some(lint) = lints.get(idx)
        {
            return Some(RenderTarget::Hover(lint));
        }
        None
    }
}

/// What the renderer is about to draw.
#[derive(Debug)]
pub enum RenderTarget<'a> {
    /// User pressed Tab — interactive picker with the user's selection.
    Engaged(&'a PickerSnapshot),
    /// Cursor idle inside a lint — passive tooltip showing the top
    /// suggestion (no selection).
    Hover(&'a SpellIssue),
}

/// Pick the lint to anchor the picker on, given the buffer cursor and
/// the current lint set. Used by both hover (which lint is the cursor
/// on?) and Tab-engage (which lint should the picker fix?). Returns the
/// last-typed actionable spelling/grammar lint that the cursor is at-or-past.
///
/// Rules:
/// - Lint's category must be actionable per
///   [`crate::spell::is_actionable_category`] (Spelling unconditionally,
///   Grammar when `grammar_enabled`) AND have non-empty suggestions.
/// - The cursor must be inside the span OR exactly at `char_end` (i.e.,
///   just past the word, before the next separator — matches the
///   "I just finished typing this and want to fix it" intent).
/// - When multiple lints satisfy, prefer the rightmost (highest
///   `char_start`), matching "fix the most recent thing I noticed."
pub fn pick_target(
    lints: &[SpellIssue],
    buffer_cursor: usize,
    grammar_enabled: bool,
) -> Option<&SpellIssue> {
    let mut best: Option<&SpellIssue> = None;
    for lint in lints {
        if !crate::spell::is_actionable_category(lint.category, grammar_enabled) {
            continue;
        }
        if lint.suggestions.is_empty() {
            continue;
        }
        if buffer_cursor < lint.char_start || buffer_cursor > lint.char_end {
            continue;
        }
        match best {
            None => best = Some(lint),
            Some(prev) if lint.char_start > prev.char_start => best = Some(lint),
            _ => {}
        }
    }
    best
}

/// True when the user is typing forward at the tail of the buffer — the
/// cursor sits at/after the last character and the buffer didn't just
/// shrink. In this state both the hover tooltip and the Tab-engage path
/// stay out of the way: forward typing at end-of-buffer is exactly where
/// the wrapped child wants Tab for its own completion (shell `cd foo<Tab>`,
/// vim indent, slash-command pickers), so interfering would break it (see
/// decision #11). The picker only activates once the user shows *edit
/// intent* — moves the cursor back into a finished word (`cursor < chars`)
/// or deletes into the trailing word (`buffer_shrank`).
///
/// Shared by [`check_hover`] (render side, reads from the matcher) and the
/// stdin pump's `picker_engage` (reads from `InputState`) so the two can't
/// drift — the invariant is "Tab engages iff the hover tooltip is, or would
/// be, showing."
pub fn typing_forward_at_end(cursor: usize, chars: usize, buffer_shrank: bool) -> bool {
    cursor >= chars && !buffer_shrank
}

/// Build the bytes that paint the picker overlay AND restore the
/// previously-drawn region (if any) if it falls outside the new one.
/// Returns (bytes, new_region). When the picker should not render
/// (no suggestions, no room, anchor lookup failed) the bytes
/// re-write the cells from the grid to clear any previous overlay and
/// `new_region` is None.
pub fn build_overlay(
    target: &RenderTarget<'_>,
    matcher: &EchoMatcher,
    screen: &ScreenState,
    prior_region: Option<OverlayRegion>,
) -> (Vec<u8>, Option<OverlayRegion>) {
    let (target_word, suggestions, selected, engaged) = match target {
        RenderTarget::Engaged(snap) => (
            snap.target.as_str(),
            snap.suggestions.as_slice(),
            snap.selected,
            true,
        ),
        RenderTarget::Hover(lint) => (
            lint.word.as_str(),
            lint.suggestions.as_slice(),
            0usize,
            false,
        ),
    };

    if suggestions.is_empty() {
        let bytes = build_restore(prior_region, matcher, screen);
        return (bytes, None);
    }

    let Some(anchor) = matcher.find_input_anchor(screen.cols) else {
        let bytes = build_restore(prior_region, matcher, screen);
        return (bytes, None);
    };

    // Anchor is the column of the first char of the trailing line of
    // buffer text. The misspelled word sits at
    // `anchor.col + (char_start - last_line_offset)..(char_end - last_line_offset)`
    // — we need to know `char_start` to anchor the overlay correctly. We re-
    // derive it from the matcher's lints by looking up the matching word.
    // (Engaged carries char_start/char_end in stdin's Engaged struct; hover
    // already has the lint. For both we can match by word + position via
    // matcher.lints.)
    let lint = matcher
        .lints()
        .iter()
        .find(|l| l.word == target_word)
        .or_else(|| matcher.lints().first());
    let char_start = lint.map(|l| l.char_start).unwrap_or(0);
    let char_end = lint.map(|l| l.char_end).unwrap_or(target_word.chars().count());

    // Multi-line buffer support: last_line_offset > 0 when the buffer
    // contains '\n' (typical after a bracketed paste). The anchor is
    // for the trailing line; lint offsets are global, so we subtract
    // to make them last-line-local. For lints that fall in an earlier
    // line, the column math would underflow — skip and bail out
    // (the painter applies the same filter; the picker should match).
    let last_line_offset = matcher.last_line_char_offset();
    if char_start < last_line_offset {
        let bytes = build_restore(prior_region, matcher, screen);
        return (bytes, None);
    }
    let local_char_start = char_start - last_line_offset;
    let local_char_end = char_end - last_line_offset;

    let word_col_start = anchor.col.saturating_add_signed(local_char_start as i16);
    let word_col_end = anchor.col.saturating_add_signed(local_char_end as i16);
    let anchor_row = anchor.row;

    let content = render_content(suggestions, selected, engaged);
    let content_width = content.visible_chars as u16;

    // Try inline-right first: just after the word + a 1-col gap.
    let inline_col = word_col_end.saturating_add(1);
    let (place_row, place_col) = if inline_col + content_width <= screen.cols
        && anchor_row < screen.rows
    {
        (anchor_row, inline_col)
    } else if anchor_row > 0 && word_col_start + content_width <= screen.cols {
        // Fall back to the row above the anchor, left-aligned with the word.
        (anchor_row - 1, word_col_start)
    } else {
        // No safe place. Skip and restore any prior overlay.
        let bytes = build_restore(prior_region, matcher, screen);
        return (bytes, None);
    };

    let new_region = OverlayRegion {
        row: place_row,
        col_start: place_col,
        col_end: place_col + content_width,
    };

    let cur_row = screen.cursor.row.min(screen.rows.saturating_sub(1));
    let cur_col = screen.cursor.col.min(screen.cols.saturating_sub(1));

    let mut out = Vec::new();
    let mut pen = (cur_row, cur_col);

    // Restore any cells from the previous overlay region that aren't
    // covered by the new one (e.g., we moved the picker, or the word
    // got shorter so trailing cells need to go back to host content).
    if let Some(prev) = prior_region
        && prev != new_region
    {
        restore_cells_into(&mut out, &mut pen, prev, matcher, Some(new_region));
    }

    // Paint the overlay.
    paint::emit_relative_move(&mut out, pen, (place_row, place_col));
    // Dim cyan ambient color, same vocabulary as the status row.
    out.extend_from_slice(b"\x1b[2;36m");
    out.extend_from_slice(content.bytes.as_slice());
    out.extend_from_slice(b"\x1b[0m");
    pen = (place_row, place_col + content_width);

    paint::emit_relative_move(&mut out, pen, (cur_row, cur_col));

    (out, Some(new_region))
}

/// Build only the restore bytes for a region, with no paint pass. Used
/// when the picker dismisses entirely.
pub fn build_restore(
    region: Option<OverlayRegion>,
    matcher: &EchoMatcher,
    screen: &ScreenState,
) -> Vec<u8> {
    let Some(region) = region else {
        return Vec::new();
    };
    let cur_row = screen.cursor.row.min(screen.rows.saturating_sub(1));
    let cur_col = screen.cursor.col.min(screen.cols.saturating_sub(1));
    let mut out = Vec::new();
    let mut pen = (cur_row, cur_col);
    restore_cells_into(&mut out, &mut pen, region, matcher, None);
    if out.is_empty() {
        return Vec::new();
    }
    paint::emit_relative_move(&mut out, pen, (cur_row, cur_col));
    out
}

/// Rewrite the cells of `region` from the grid with `\x1b[m` (full SGR
/// reset). If `keep` is set, cells covered by `keep` are skipped — they
/// belong to the new overlay and we shouldn't double-restore them.
fn restore_cells_into(
    out: &mut Vec<u8>,
    pen: &mut (u16, u16),
    region: OverlayRegion,
    matcher: &EchoMatcher,
    keep: Option<OverlayRegion>,
) {
    if region.col_end <= region.col_start {
        return;
    }
    // Build the run of chars; skip cells the keeper covers (same row).
    let mut col = region.col_start;
    while col < region.col_end {
        if let Some(k) = keep
            && k.row == region.row
            && col >= k.col_start
            && col < k.col_end
        {
            col = k.col_end;
            continue;
        }
        let run_start = col;
        let mut chars = String::new();
        while col < region.col_end {
            if let Some(k) = keep
                && k.row == region.row
                && col >= k.col_start
                && col < k.col_end
            {
                break;
            }
            chars.push(matcher.cell_at(region.row, col).unwrap_or(' '));
            col += 1;
        }
        if chars.is_empty() {
            continue;
        }
        paint::emit_relative_move(out, *pen, (region.row, run_start));
        // Full reset so we drop the dim-cyan + reverse SGR we set.
        out.extend_from_slice(b"\x1b[m");
        out.extend_from_slice(chars.as_bytes());
        *pen = (region.row, run_start + chars.chars().count() as u16);
    }
}

/// Visible content + raw bytes (with embedded SGR for reverse-video on
/// the selected suggestion). Bytes are written verbatim; visible_chars
/// is the cell width so the layout math can compute placement.
struct RenderedContent {
    bytes: Vec<u8>,
    visible_chars: usize,
}

fn render_content(
    suggestions: &[String],
    selected: usize,
    engaged: bool,
) -> RenderedContent {
    // Format: [selected] other1 other2 ... <gap><hint>
    let hint: &str = if engaged {
        // Hint reflects the engaged-mode keybinding model:
        //   - Tab or Left/Right (also Up/Down) cycle the highlight.
        //   - Enter commits.
        //   - Esc cancels.
        // Kept compact so narrow terminals still have room for the
        // suggestion list itself.
        "(tab/←→ · enter · esc)"
    } else {
        "(tab to pick)"
    };
    let mut bytes = Vec::new();
    let mut visible = 0usize;
    bytes.push(b' ');
    visible += 1;
    for (i, sugg) in suggestions.iter().enumerate() {
        if i > 0 {
            bytes.push(b' ');
            visible += 1;
        }
        if i == selected && engaged {
            // Reverse-video brackets so selection pops even on terminals
            // without dim-color support.
            bytes.extend_from_slice(b"\x1b[7m[");
            bytes.extend_from_slice(sugg.as_bytes());
            bytes.extend_from_slice(b"]\x1b[27m");
            visible += sugg.chars().count() + 2;
        } else if i == selected {
            // Hover: highlight the would-be choice via brackets only.
            bytes.push(b'[');
            bytes.extend_from_slice(sugg.as_bytes());
            bytes.push(b']');
            visible += sugg.chars().count() + 2;
        } else {
            bytes.extend_from_slice(sugg.as_bytes());
            visible += sugg.chars().count();
        }
    }
    bytes.extend_from_slice(b"  ");
    visible += 2;
    bytes.extend_from_slice(hint.as_bytes());
    visible += hint.chars().count();
    bytes.push(b' ');
    visible += 1;
    RenderedContent {
        bytes,
        visible_chars: visible,
    }
}

/// Write the picker overlay to `out`. Returns the new region (or None if
/// nothing was painted) and the bytes that were written for debug/test.
pub fn paint_overlay<W: Write>(
    out: &mut W,
    gate: &mut OverlayGate,
    matcher: &EchoMatcher,
    screen: &ScreenState,
) -> usize {
    if !crate::config::get().picker_enabled() {
        // Picker disabled — restore any leftover overlay (could happen
        // mid-session if the user edits the config; harmless if not).
        let bytes = build_restore(gate.region, matcher, screen);
        gate.region = None;
        if !bytes.is_empty() {
            let _ = out.write_all(&bytes);
            let _ = out.flush();
            return bytes.len();
        }
        return 0;
    }
    let lints = matcher.lints().to_vec();
    let Some(target) = gate.snapshot_to_render(&lints) else {
        // Nothing to show — restore prior overlay.
        let bytes = build_restore(gate.region, matcher, screen);
        gate.region = None;
        if bytes.is_empty() {
            return 0;
        }
        let _ = out.write_all(&bytes);
        let _ = out.flush();
        return bytes.len();
    };
    let (bytes, new_region) = build_overlay(&target, matcher, screen, gate.region);
    gate.region = new_region;
    if bytes.is_empty() {
        return 0;
    }
    let _ = out.write_all(&bytes);
    let _ = out.flush();
    bytes.len()
}

/// Evaluate the hover trigger. Returns true if the hover state changed
/// (so the caller knows to repaint). Should be called from the render
/// loop on tick.
///
/// ## When hover is allowed to fire
///
/// Two situations open the gate:
///
/// 1. **Cursor moved back inside the buffer** (`buffer_cursor <
///    buffer_chars`). Arrow-back or click-to-position; the user is
///    deliberately on a finished word.
/// 2. **Cursor at end-of-buffer AND the buffer just shrank.** The user
///    typed past the word, then deleted forward off the boundary
///    (backspace / Ctrl-W / Delete) and is now inspecting the trailing
///    word. Without this branch, "type `write teh `, backspace once"
///    leaves the cursor on `teh` with no picker — the user has just
///    erased the separator they typed to *finish* the word.
///
/// ## Why end-of-buffer is suppressed during forward typing
///
/// When the user is typing forward, their cursor sits at end-of-buffer
/// (`buffer_cursor == buffer_chars`) by construction. Without a gate,
/// hover would pop up every time the user paused after typing a
/// misspelling — and would also fight shell-side completion (e.g. zsh's
/// tab suggestion of `npm run dev`). The shrink signal is the
/// distinguishing feature: forward typing grows `buffer_chars`,
/// deletion shrinks it. Once growth resumes the flag flips back.
pub fn check_hover(
    gate: &mut OverlayGate,
    matcher: &EchoMatcher,
    now: Instant,
    hover_window: std::time::Duration,
) -> bool {
    if !crate::config::get().picker_enabled() {
        if gate.hover_lint.take().is_some() {
            return true;
        }
        return false;
    }
    if gate.engaged_snapshot.is_some() {
        // Engaged mode wins; hover doesn't apply.
        if gate.hover_lint.take().is_some() {
            return true;
        }
        return false;
    }
    // Suppress hover only while the cursor is at end-of-buffer AND the
    // user is typing forward (no recent shrink). Backspace-edit at the
    // tail of the buffer opens the gate — see the doc comment above.
    if typing_forward_at_end(matcher.buffer_cursor(), matcher.buffer_chars(), gate.buffer_shrank) {
        if gate.hover_lint.take().is_some() {
            return true;
        }
        return false;
    }
    let idle_ok = gate
        .cursor_changed_at
        .is_some_and(|t| now.duration_since(t) >= hover_window);
    if !idle_ok {
        if gate.hover_lint.take().is_some() {
            return true;
        }
        return false;
    }
    let grammar_enabled = crate::config::get().grammar_enabled();
    let next = matcher.lint_at_cursor(grammar_enabled);
    if next != gate.hover_lint {
        gate.hover_lint = next;
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{InputEvent, PrintEvent};
    use crate::screen::CursorPos;
    use crate::spell::IssueCategory;

    fn issue(start: usize, end: usize, word: &str, suggestions: &[&str]) -> SpellIssue {
        SpellIssue {
            byte_start: start,
            byte_end: end,
            char_start: start,
            char_end: end,
            word: word.into(),
            message: "test".into(),
            suggestions: suggestions.iter().map(|s| (*s).into()).collect(),
            category: IssueCategory::Spelling,
            priority: 50,
        }
    }

    fn screen(cols: u16, rows: u16, cur: CursorPos) -> ScreenState {
        let mut s = ScreenState::new(cols, rows);
        s.cursor = cur;
        s
    }

    fn feed_history(m: &mut EchoMatcher, row: u16, start_col: u16, text: &str) {
        for (i, ch) in text.chars().enumerate() {
            m.apply_print(&PrintEvent {
                ch,
                at: CursorPos {
                    row,
                    col: start_col + i as u16,
                },
            });
        }
    }

    #[test]
    fn engage_returns_none_when_no_suggestions() {
        assert!(engage_for(&issue(0, 3, "teh", &[])).is_none());
    }

    #[test]
    fn nav_wraps_around_in_both_directions() {
        let mut e = engage_for(&issue(0, 3, "teh", &["the", "ten", "tech"])).unwrap();
        assert_eq!(e.selected, 0);
        e.nav(1);
        assert_eq!(e.selected, 1);
        e.nav(1);
        e.nav(1);
        assert_eq!(e.selected, 0, "wraps forward");
        e.nav(-1);
        assert_eq!(e.selected, 2, "wraps backward");
    }

    #[test]
    fn pick_target_chooses_rightmost_actionable() {
        // Two lints; cursor is past both. Pick the later one.
        let lints = vec![
            issue(0, 3, "teh", &["the"]),
            issue(4, 13, "paragprah", &["paragraph"]),
        ];
        let picked = pick_target(&lints, 13, false).expect("should pick");
        assert_eq!(picked.word, "paragprah");
    }

    #[test]
    fn pick_target_ignores_unactionable() {
        let lints = vec![issue(0, 3, "teh", &[])]; // no suggestions
        assert!(pick_target(&lints, 3, false).is_none());
    }

    #[test]
    fn pick_target_inside_span_counts() {
        let lints = vec![issue(0, 6, "reacon", &["reason"])];
        // Cursor INSIDE the span, not just at end.
        let picked = pick_target(&lints, 2, false).expect("should pick");
        assert_eq!(picked.word, "reacon");
    }

    #[test]
    fn pick_target_skips_when_cursor_outside() {
        let lints = vec![issue(0, 3, "teh", &["the"])];
        // Cursor past the end of the lint (with text between).
        assert!(pick_target(&lints, 10, false).is_none());
    }

    #[test]
    fn pick_target_respects_grammar_gate() {
        use crate::spell::IssueCategory;
        // A grammar-category lint at the cursor. With grammar disabled
        // it must NOT be picked; with grammar enabled it should be.
        let mut grammar_lint = issue(0, 3, "are", &["is"]);
        grammar_lint.category = IssueCategory::Grammar;
        let lints = vec![grammar_lint];
        assert!(
            pick_target(&lints, 2, false).is_none(),
            "grammar disabled → grammar lint must not be picked"
        );
        let picked = pick_target(&lints, 2, true).expect("grammar enabled → pick");
        assert_eq!(picked.word, "are");
    }

    #[test]
    fn render_content_lists_suggestions_with_selected_highlighted() {
        let suggs = vec!["the".into(), "ten".into(), "tech".into()];
        let c = render_content(&suggs, 1, true);
        let s = String::from_utf8(c.bytes.clone()).unwrap();
        // Selected is index 1 → "ten" wrapped in reverse video.
        assert!(s.contains("\x1b[7m[ten]\x1b[27m"), "missing reverse: {s:?}");
        // Other suggestions appear plainly.
        assert!(s.contains("the"));
        assert!(s.contains("tech"));
        // Engaged hint mentions Enter to commit.
        assert!(s.contains("enter"));
    }

    #[test]
    fn render_content_passive_uses_brackets_not_reverse() {
        let suggs = vec!["the".into(), "ten".into()];
        let c = render_content(&suggs, 0, false);
        let s = String::from_utf8(c.bytes.clone()).unwrap();
        assert!(s.contains("[the]"));
        assert!(!s.contains("\x1b[7m"), "passive should not use reverse: {s:?}");
        assert!(s.contains("tab to pick"));
    }

    #[test]
    fn build_overlay_renders_inline_right_when_room() {
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh", &["the", "ten"])],
            buffer_chars: 4,
            buffer_text: "teh ".into(),
            buffer_cursor: 3,
        });
        feed_history(&mut m, 5, 10, "teh ");
        let s = screen(120, 24, CursorPos { row: 5, col: 14 });
        let snap = PickerSnapshot {
            target: "teh".into(),
            suggestions: vec!["the".into(), "ten".into()],
            selected: 0,
        };
        let (bytes, region) =
            build_overlay(&RenderTarget::Engaged(&snap), &m, &s, None);
        assert!(!bytes.is_empty());
        let region = region.expect("expected a region");
        // Word ends at col 13 (anchor 10 + char_end 3). Inline = 14.
        assert_eq!(region.row, 5);
        assert_eq!(region.col_start, 14);
        let raw = String::from_utf8_lossy(&bytes);
        assert!(raw.contains("\x1b[7m[the]\x1b[27m"));
    }

    #[test]
    fn build_overlay_falls_back_above_when_inline_too_wide() {
        // Narrow screen + 2 suggestions: with the engaged hint
        // (~22 chars), content is ~35 chars. Choose geometry so inline
        // doesn't fit but the row above (word_col_start..+content) does.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh", &["the", "ten"])],
            buffer_chars: 4,
            buffer_text: "teh ".into(),
            buffer_cursor: 3,
        });
        feed_history(&mut m, 5, 10, "teh ");
        // cols = 45 → inline-right (col 14..49) exceeds; fallback above
        // (col 10..45) fits exactly.
        let s = screen(45, 24, CursorPos { row: 5, col: 14 });
        let snap = PickerSnapshot {
            target: "teh".into(),
            suggestions: vec!["the".into(), "ten".into()],
            selected: 0,
        };
        let (_, region) = build_overlay(&RenderTarget::Engaged(&snap), &m, &s, None);
        let region = region.expect("expected a region");
        assert_eq!(region.row, 4, "should fall back to row above");
        assert_eq!(region.col_start, 10, "aligned with word start on fallback");
    }

    #[test]
    fn build_overlay_skips_when_no_room_above_or_right() {
        // Lint on row 0, screen too narrow for inline. No fallback.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh", &["the", "ten", "tech"])],
            buffer_chars: 4,
            buffer_text: "teh ".into(),
            buffer_cursor: 3,
        });
        feed_history(&mut m, 0, 0, "teh ");
        let s = screen(10, 24, CursorPos { row: 0, col: 3 });
        let snap = PickerSnapshot {
            target: "teh".into(),
            suggestions: vec!["the".into(), "ten".into(), "tech".into()],
            selected: 0,
        };
        let (_, region) = build_overlay(&RenderTarget::Engaged(&snap), &m, &s, None);
        assert!(region.is_none(), "no safe place to render");
    }

    #[test]
    fn build_restore_writes_grid_chars_with_full_reset() {
        let mut m = EchoMatcher::new();
        feed_history(&mut m, 5, 10, "hello world");
        let s = screen(80, 24, CursorPos { row: 5, col: 21 });
        let region = OverlayRegion {
            row: 5,
            col_start: 16,
            col_end: 21,
        };
        let bytes = build_restore(Some(region), &m, &s);
        let raw = String::from_utf8_lossy(&bytes);
        assert!(raw.contains("\x1b[m"), "missing full SGR reset: {raw:?}");
        assert!(raw.contains("world"), "missing grid chars: {raw:?}");
    }

    #[test]
    fn typing_forward_at_end_gates_picker() {
        // The shared gate used by both hover and Tab-engage. The whole
        // point is that `cd jepa<Tab>` (cursor at end, typing forward)
        // must NOT engage the picker so shell completion still runs.
        // Typing forward at end-of-buffer → suppressed.
        assert!(typing_forward_at_end(7, 7, false), "cd jepa<Tab>: suppress");
        assert!(typing_forward_at_end(9, 7, false), "cursor past end: suppress");
        // Cursor moved back into a finished word → allowed.
        assert!(!typing_forward_at_end(2, 7, false), "cursor inside buffer");
        // At end-of-buffer but just shrank (backspaced into trailing word)
        // → allowed, matching the hover relaxation.
        assert!(!typing_forward_at_end(7, 7, true), "shrink relaxes the gate");
    }

    #[test]
    fn check_hover_does_not_fire_while_user_types_at_end_of_buffer() {
        // Regression for the "auto-suggestion popping" bug: the picker
        // hover used to fire whenever the cursor sat inside any lint span,
        // including the in-progress word being typed. That fought
        // shell-side completion (npm → npm run dev) and made typing feel
        // intrusive. Hover should only fire when the user has moved the
        // cursor BACK into a finished word — i.e. when `buffer_cursor <
        // buffer_chars`.
        let _ = super::super::config::GLOBAL.set(crate::config::Config {
            picker: true,
            ..crate::config::Config::default()
        });
        if !crate::config::get().picker_enabled() {
            return; // another parallel test won the OnceLock race
        }

        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh", &["the"])],
            buffer_chars: 3,
            // Cursor at end of buffer (still typing `teh`).
            buffer_text: "teh".into(),
            buffer_cursor: 3,
        });
        feed_history(&mut m, 0, 0, "teh");

        let mut gate = OverlayGate::new();
        let t0 = Instant::now();
        gate.note_buffer(3, 3, t0);
        let later = t0 + std::time::Duration::from_millis(300);
        let changed = check_hover(&mut gate, &m, later, std::time::Duration::from_millis(250));
        assert!(!changed, "hover should be silent at end-of-buffer");
        assert!(gate.hover_lint.is_none());
    }

    #[test]
    fn check_hover_fires_after_idle_window() {
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh", &["the"])],
            buffer_chars: 4,
            buffer_text: "teh ".into(),
            buffer_cursor: 2,
        });
        feed_history(&mut m, 0, 0, "teh ");

        // Picker must be enabled. Set it via the OnceLock (test-only; in
        // production this is set once at startup from main).
        // Note: parallel tests may have already locked the global. We
        // skip the test if our set fails — the assertion below would
        // still be true on success but the global state is uncertain.
        let _ = super::super::config::GLOBAL.set(crate::config::Config {
            picker: true,
            ..crate::config::Config::default()
        });
        if !crate::config::get().picker_enabled() {
            // Another test got there first with picker=false. Bail.
            return;
        }

        let mut gate = OverlayGate::new();
        let t0 = Instant::now();
        gate.note_buffer(2, 4, t0);
        // Before idle window: no hover.
        assert!(!check_hover(&mut gate, &m, t0, std::time::Duration::from_millis(250)));
        assert!(gate.hover_lint.is_none());
        // After idle window: hover lights up.
        let later = t0 + std::time::Duration::from_millis(300);
        let changed = check_hover(&mut gate, &m, later, std::time::Duration::from_millis(250));
        assert!(changed);
        assert_eq!(gate.hover_lint, Some(0));
    }

    #[test]
    fn check_hover_fires_at_end_of_buffer_after_backspace_into_flagged_word() {
        // Scenario: user typed "write teh " (buffer_chars=10), saw the
        // underline, then backspaced once — buffer is now "write teh"
        // (buffer_chars=9, cursor=9). The shrink signal opens the hover
        // gate even though the cursor sits at end-of-buffer. Without
        // this branch, the user has no way to invoke the picker on the
        // last word short of typing a separator and then backspacing
        // into the middle of the word.
        let _ = super::super::config::GLOBAL.set(crate::config::Config {
            picker: true,
            ..crate::config::Config::default()
        });
        if !crate::config::get().picker_enabled() {
            return; // another parallel test won the OnceLock race
        }

        let mut m = EchoMatcher::new();
        // Initial state: "write teh " (10 chars), cursor at end.
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(6, 9, "teh", &["the"])],
            buffer_chars: 10,
            buffer_text: "write teh ".into(),
            buffer_cursor: 10,
        });
        feed_history(&mut m, 0, 0, "write teh ");

        let mut gate = OverlayGate::new();
        let t0 = Instant::now();
        gate.note_buffer(10, 10, t0);
        // First state: no shrink yet, cursor at end → silent.
        let later1 = t0 + std::time::Duration::from_millis(300);
        let changed = check_hover(&mut gate, &m, later1, std::time::Duration::from_millis(250));
        assert!(!changed, "no hover while typing forward");
        assert!(!gate.buffer_shrank);

        // Backspace: buffer "write teh" (9 chars), cursor at 9.
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(6, 9, "teh", &["the"])],
            buffer_chars: 9,
            buffer_text: "write teh".into(),
            buffer_cursor: 9,
        });
        let t1 = later1 + std::time::Duration::from_millis(50);
        gate.note_buffer(9, 9, t1);
        assert!(gate.buffer_shrank, "shrink should be detected");

        // Before idle window: no hover (250ms gate).
        assert!(!check_hover(
            &mut gate,
            &m,
            t1,
            std::time::Duration::from_millis(250)
        ));
        // After idle window: hover fires on "teh".
        let t2 = t1 + std::time::Duration::from_millis(300);
        let changed = check_hover(&mut gate, &m, t2, std::time::Duration::from_millis(250));
        assert!(changed, "hover should fire after backspace + idle");
        assert_eq!(gate.hover_lint, Some(0));
    }

    #[test]
    fn check_hover_resets_shrink_after_subsequent_growth() {
        // After a backspace-shrink, a forward keystroke must flip the
        // gate closed again — otherwise the picker would keep popping
        // up while the user resumes typing after a brief edit.
        let _ = super::super::config::GLOBAL.set(crate::config::Config {
            picker: true,
            ..crate::config::Config::default()
        });
        if !crate::config::get().picker_enabled() {
            return;
        }

        let mut gate = OverlayGate::new();
        let t0 = Instant::now();
        // Walk through: write teh (9 chars, cursor=9) → backspace nothing
        // here, just the initial state — then add a space (grow), then
        // backspace (shrink), then type a char (grow again). After the
        // final grow, buffer_shrank should be false.
        gate.note_buffer(9, 9, t0);
        gate.note_buffer(10, 10, t0); // grew
        assert!(!gate.buffer_shrank);
        gate.note_buffer(9, 9, t0); // shrank
        assert!(gate.buffer_shrank);
        gate.note_buffer(10, 10, t0); // grew again
        assert!(!gate.buffer_shrank, "growth should reset the shrink flag");
    }

    #[test]
    fn reset_buffer_tracking_prevents_false_shrink_after_boundary() {
        // Without resetting, a boundary (Enter) leaves
        // `last_buffer_chars = Some(N)`. The next prompt's first
        // keystroke arrives with `buffer_chars = 1`, which would
        // register as a shrink (1 < N) and open the hover gate on the
        // very first char of the new prompt.
        let mut gate = OverlayGate::new();
        let t0 = Instant::now();
        gate.note_buffer(10, 10, t0);
        // Simulate Boundary handling in pty.rs.
        gate.reset_buffer_tracking();
        // Next prompt's first keystroke.
        gate.note_buffer(1, 1, t0);
        assert!(
            !gate.buffer_shrank,
            "post-boundary first keystroke must not register as shrink"
        );
    }
}
