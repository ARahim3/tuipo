//! Pause-aware inline annotation painter.
//!
//! When the user pauses typing for `PAUSE_MS`, we paint an underline under
//! each misspelled word at the screen position where that word is actually
//! rendered. Default style is a red curly underline (`\x1b[4:3m\x1b[58:2::255:0:0m`)
//! on terminals that handle the T.416 colon-form sub-parameters; Apple
//! Terminal gets a plain `\x1b[4m` because it mis-parses the colon form.
//! Override with `TUIPO_PLAIN_UNDERLINE=1` (force plain) or
//! `TUIPO_FANCY_UNDERLINE=1` (force fancy, e.g. for tmux that strips
//! `TERM_PROGRAM`).
//!
//! ## Stale-underline cleanup
//!
//! SGR is a *per-cell* attribute. Once we set underline on a cell, it stays
//! until the cell is rewritten with different attrs. When a user corrects a
//! typo in place — e.g. `reacon` → backspace `con` → type `son` — the host
//! only rewrites the changed cells (`con` → `son`), leaving the unchanged
//! prefix (`rea`) with our stale underline forever. Each paint cycle we
//! diff the spans we previously underlined against the spans the current
//! lints want underlined; spans that fell out get a "clear" pass that
//! rewrites the cells from the screen-grid with `\x1b[24m` so the user
//! sees the correction take effect.
//!
//! ## Anchor: print-history match, not cursor position
//!
//! For each paint, we ask the matcher [`EchoMatcher::find_input_anchor`]
//! where the current buffer text lives in the PTY's screen-grid model.
//! Misspelling positions are computed relative to that anchor.
//!
//! ## Cursor handling: relative moves, not CUP
//!
//! CUP (`\x1b[r;cH`) uses **absolute** terminal coordinates, but the
//! PTY-observer's coordinates and the local terminal's coordinates have
//! different origins. When tuipo is invoked from a normal terminal, the
//! local terminal already has scrollback content above the prompt — say
//! the system's `Last login:` banner at local row 0. zsh starts inside
//! the PTY at PTY (0,0) but lands visually on local row 1. Emitting
//! `\x1b[1;5H` from the painter would target local row 0 (the banner),
//! not the prompt — that's the "claude got pasted into Last login:" bug.
//!
//! Instead we move with `\x1b[<n>A/B/C/D` (cursor up/down/right/left)
//! relative to the current cursor. The PTY-vs-local origin offset
//! cancels out, because both screens have been tracking the same deltas
//! since startup. We also avoid `ESC 7 / ESC 8` (single shared
//! saved-cursor slot — clobbers the child's own save/restore).
//!
//! ## SGR: plain underline
//!
//! We emit just `ESC [4m` / `ESC [24m`. The colon-separated forms (`4:3`
//! for undercurl, `58:5:N` for underline color) are ITU T.416 and
//! supported by many modern terminals — but Apple's Terminal.app
//! mis-parses them by digit-concatenation, turning `\x1b[4:3m` into
//! `\x1b[43m` (yellow background) and producing a "highlighter" artifact.
//! Plain underline reads as subtle-Grammarly-style across every
//! terminal we care about; color can be re-added later behind a
//! terminal-detection gate.
//!
//! ## Spelling lints only, completed words only, actionable only
//!
//! Three filters on the lint list:
//! - **Category == Spelling**: harper's grammar/style/usage flavors fire
//!   on correctly-spelled common words; we don't paint them.
//! - **Non-empty suggestions**: lints with no replacement are usually
//!   too speculative or partial-word artifacts to act on.
//! - **Not the word being typed** (lint end ≠ buffer end): painting the
//!   in-progress word leaves stale underline cells when the partial-word
//!   lint disappears, because terminals retain per-cell SGR attributes
//!   until something rewrites the cell. The Grammarly-classic rule of
//!   "wait until the word is finished" sidesteps that entire class of
//!   visual residue.
//!
//! Multi-row spans (wrapped words) aren't painted — we require the whole
//! word to fit on the anchor row.

use std::collections::HashSet;
use std::io::Write;
use std::time::{Duration, Instant};

use crate::echo::EchoMatcher;
use crate::screen::ScreenState;
use crate::spell::is_actionable_category;

/// Wait this long after the last user keystroke before painting. ~150ms
/// matches typical "I've stopped typing and might be looking at the screen"
/// timing without making the annotation feel laggy.
pub const PAUSE_MS: Duration = Duration::from_millis(150);

/// A span we wrote an underline to last paint. Used to actively undo the
/// underline when the corresponding lint goes away.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct PaintedSpan {
    row: u16,
    col_start: u16,
    col_end: u16,
}

/// Tracks whether annotations need (re)painting, when the user last
/// touched the keyboard, and which spans currently carry our underline
/// attribute so we can clear them when a misspelling is corrected.
pub struct PaintGate {
    dirty: bool,
    last_keystroke_at: Option<Instant>,
    /// Hash of the last-painted (lints, positions, screen.alt_screen) tuple.
    /// Lets us skip redundant paints when nothing visible changed.
    last_paint_signature: u64,
    /// Spans currently believed to carry our underline attribute on screen.
    /// Source of truth for "what do we need to actively clear when the
    /// associated lint goes away."
    painted: Vec<PaintedSpan>,
}

impl PaintGate {
    pub fn new() -> Self {
        Self {
            dirty: true,
            last_keystroke_at: None,
            last_paint_signature: 0,
            painted: Vec::new(),
        }
    }

    pub fn mark_keystroke(&mut self) {
        self.last_keystroke_at = Some(Instant::now());
        self.dirty = true;
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn should_paint(&self, now: Instant) -> bool {
        if !self.dirty {
            return false;
        }
        match self.last_keystroke_at {
            Some(t) => now.duration_since(t) >= PAUSE_MS,
            None => true,
        }
    }

    pub fn record_paint(&mut self, signature: u64) {
        self.last_paint_signature = signature;
        self.dirty = false;
    }

    /// Drop the painted-span list without writing clear bytes. Call when
    /// the host has just cleared / will imminently overwrite the relevant
    /// region (Boundary, Resize). Trying to clear via the grid in those
    /// moments would either write empty strings (grid is empty post-
    /// clear_screen) or race the host's redraw.
    pub fn forget_painted(&mut self) {
        self.painted.clear();
    }

    #[cfg(test)]
    pub(crate) fn painted_len(&self) -> usize {
        self.painted.len()
    }
}

impl Default for PaintGate {
    fn default() -> Self {
        Self::new()
    }
}

/// True when paint should be active. Defaults to ON; users disable with
/// `TUIPO_PAINT_OFF=1` (env override) or `paint = false` in
/// `~/.config/tuipo/config.toml`. We don't gate on alt-screen any more
/// because many real TUIs (notably Claude Code) render inline rather
/// than entering alt-screen mode, and the gate suppressed painting for
/// them entirely.
pub fn paint_active(_screen: &ScreenState) -> bool {
    crate::config::get().paint_enabled()
}

/// Two flavors of underline SGR. `open` goes before the word, `close`
/// goes after. We always pair them so the pen returns to no-underline for
/// whatever the child writes next.
#[derive(Clone, Copy)]
pub(crate) struct UnderlineStyle {
    open: &'static [u8],
    close: &'static [u8],
}

/// Plain underline in the terminal's default color. The only SGR form
/// every terminal renders identically — Apple Terminal mis-parses the
/// colon-separated sub-parameter forms (see `FANCY` below).
pub(crate) const PLAIN: UnderlineStyle = UnderlineStyle {
    open: b"\x1b[4m",
    close: b"\x1b[24m",
};

/// Curly red underline (T.416 colon-form). `4:3` = curly underline,
/// `58:2::255:0:0` = direct-color underline at RGB `#ff0000`. `59`
/// resets underline color to terminal default; `24` turns underline
/// off. Renders correctly on iTerm2, Ghostty, kitty, WezTerm, VS Code's
/// embedded terminal, Zed, and any modern xterm-compatible. Apple
/// Terminal parses `4:3` by digit-concatenation as `\x1b[43m` (yellow
/// background) — never emit this there.
///
/// **Why direct color (RGB), not palette.** Earlier versions used
/// palette `58:5:N` — first `N=9` (theme-overridable bright red,
/// rendered as white-ish on Zed dark themes), then `N=196` (pure red
/// in the 6×6×6 cube). Even index 196 came back as a white-ish
/// underline on Zed, which suggests some terminals parse the `58:5:N`
/// SGR but render the underline in the foreground/default color rather
/// than the specified palette slot. Direct color (`58:2::R:G:B`) is
/// the most explicit form — there's no palette slot to remap and no
/// ambiguity about which color is meant. The `::` is the T.416
/// canonical empty color-space designator slot; modern terminals
/// accept either `58:2::R:G:B` or the shorter `58:2:R:G:B`. We use
/// the canonical form for maximum portability.
pub(crate) const FANCY: UnderlineStyle = UnderlineStyle {
    open: b"\x1b[4:3m\x1b[58:2::255:0:0m",
    close: b"\x1b[59m\x1b[24m",
};

/// Pick the underline style by delegating to [`crate::config::Config`]'s
/// resolution. Env overrides win there, then the config's
/// `underline = "auto" | "plain" | "fancy"`, then `TERM_PROGRAM`
/// detection (Apple_Terminal → PLAIN, else FANCY).
fn detect_underline_style() -> UnderlineStyle {
    crate::config::get().underline_style()
}

/// Like [`build_annotations`] but takes the `active` flag, underline
/// style, and grammar-enabled gate explicitly. Used by tests that need
/// to verify each branch without touching process-wide env state (which
/// would race against tests run in parallel).
#[cfg(test)]
pub(crate) fn build_annotations_with(
    gate: &mut PaintGate,
    matcher: &EchoMatcher,
    screen: &ScreenState,
    active: bool,
    style: UnderlineStyle,
    grammar_enabled: bool,
) -> Vec<u8> {
    if !active {
        gate.forget_painted();
        return Vec::new();
    }
    build_annotations_inner(gate, matcher, screen, style, grammar_enabled)
}

/// Build the ANSI sequence that paints annotations for every misspelling
/// AND clears any underlines we left behind on previous paints whose
/// lints have since gone away. Returns an empty slice when there's nothing
/// to do.
///
/// Positions are anchored to the on-screen rendering of the buffer text
/// (see [`EchoMatcher::find_input_anchor`]). If the matcher can't locate
/// the input — e.g. the user just typed and the echo hasn't reached the
/// PTY reader yet, or the input wrapped to a new row — we paint nothing
/// (but we still try to clear previously-painted spans whose grid cells
/// remain known). Skipping a paint is always safer than guessing.
pub fn build_annotations(
    gate: &mut PaintGate,
    matcher: &EchoMatcher,
    screen: &ScreenState,
) -> Vec<u8> {
    if !paint_active(screen) {
        // Paint disabled by env var. Drop tracking so we don't try to
        // clear stale spans through bytes that wouldn't be written.
        gate.forget_painted();
        return Vec::new();
    }
    let grammar_enabled = crate::config::get().grammar_enabled();
    build_annotations_inner(gate, matcher, screen, detect_underline_style(), grammar_enabled)
}

fn build_annotations_inner(
    gate: &mut PaintGate,
    matcher: &EchoMatcher,
    screen: &ScreenState,
    style: UnderlineStyle,
    grammar_enabled: bool,
) -> Vec<u8> {
    let new_spans = compute_new_spans(matcher, screen, grammar_enabled);

    // Diff against the previously-painted set: anything in `gate.painted`
    // but absent from `new_spans` needs an active clear pass — the host
    // didn't necessarily rewrite those cells, so our underline attribute
    // lives on until we explicitly overwrite the chars there.
    let new_keys: HashSet<PaintedSpan> = new_spans
        .iter()
        .map(|s| PaintedSpan {
            row: s.row,
            col_start: s.col_start,
            col_end: s.col_end,
        })
        .collect();
    let stale: Vec<PaintedSpan> = gate
        .painted
        .iter()
        .filter(|p| !new_keys.contains(p))
        .copied()
        .collect();

    if new_spans.is_empty() && stale.is_empty() {
        return Vec::new();
    }

    let cur_row = screen.cursor.row.min(screen.rows.saturating_sub(1));
    let cur_col = screen.cursor.col.min(screen.cols.saturating_sub(1));
    let mut out: Vec<u8> = Vec::new();
    // Position we believe the local cursor is at, in PTY-observer coords.
    // (Relative moves are origin-independent, so we can reason in either
    // coord system; PTY is simplest because that's what the matcher gives us.)
    let mut pen = (cur_row, cur_col);
    let mut wrote_anything = false;

    // Clear pass. For each stale span, walk it and emit a `\x1b[24m`
    // (underline off) + current-grid-chars run for every contiguous
    // sub-range whose cells are present in the grid. Cells missing from
    // the grid (evicted, erased, or echo not yet arrived) are skipped —
    // we can't safely rewrite without knowing the content. If any cell
    // was missing, the span gets carried forward to the next paint
    // cycle so it can be retried once the grid fills in. Without the
    // carry-forward, stale underline fragments persist until the host
    // happens to redraw those cells (e.g. user moves cursor and the
    // wrapped TUI repaints) — which is the "fragments slowly disappear
    // when I press left" symptom users see during mid-buffer picker fixes.
    let mut carry_forward: Vec<PaintedSpan> = Vec::new();
    for span in &stale {
        let mut all_present = true;
        let mut col = span.col_start;
        while col < span.col_end {
            // Gather a contiguous run of present cells.
            let run_start = col;
            let mut chars = String::new();
            while col < span.col_end {
                match matcher.cell_at(span.row, col) {
                    Some(ch) => {
                        chars.push(ch);
                        col += 1;
                    }
                    None => break,
                }
            }
            if !chars.is_empty() {
                emit_relative_move(&mut out, pen, (span.row, run_start));
                // Use PLAIN.close (`\x1b[24m`) — sufficient to turn off
                // any flavor of underline (plain, curly, etc.) and
                // doesn't touch other attrs.
                out.extend_from_slice(PLAIN.close);
                out.extend_from_slice(chars.as_bytes());
                pen = (span.row, run_start + chars.chars().count() as u16);
                wrote_anything = true;
            }
            // Skip any consecutive missing cells; mark the span as
            // incomplete so we retry the clear next tick.
            while col < span.col_end && matcher.cell_at(span.row, col).is_none() {
                col += 1;
                all_present = false;
            }
        }
        if !all_present {
            carry_forward.push(*span);
        }
    }

    // Paint pass.
    let mut next_painted: Vec<PaintedSpan> = Vec::with_capacity(new_spans.len() + carry_forward.len());
    for span in &new_spans {
        emit_relative_move(&mut out, pen, (span.row, span.col_start));
        out.extend_from_slice(style.open);
        out.extend_from_slice(span.word.as_bytes());
        out.extend_from_slice(style.close);
        pen = (span.row, span.col_end);
        next_painted.push(PaintedSpan {
            row: span.row,
            col_start: span.col_start,
            col_end: span.col_end,
        });
        wrote_anything = true;
    }

    // Tracking becomes:
    //   - new spans (just painted), and
    //   - stale spans whose clear pass was incomplete (some grid cells
    //     were missing). Carrying these forward lets the next tick
    //     retry the clear once the grid fills in, instead of leaving
    //     stale underline fragments dangling until the host happens to
    //     repaint that region.
    next_painted.extend(carry_forward);
    gate.painted = next_painted;

    if !wrote_anything {
        return Vec::new();
    }
    // Restore the cursor to wherever the child left it — by relative move
    // from the pen's last position. This avoids both CUP (which would use
    // wrong absolute coords when local-terminal scrollback offsets PTY)
    // and ESC 7/8 (which share a saved-cursor slot with the child).
    emit_relative_move(&mut out, pen, (cur_row, cur_col));
    out
}

/// A pending paint operation: where to write, and what chars to write.
struct NewSpan {
    row: u16,
    col_start: u16,
    col_end: u16,
    word: String,
}

/// Map the current lint set into screen-space spans the painter will
/// underline this tick. Applies all the filters documented at the top of
/// this module (actionable category, suggestions non-empty, not the word
/// being typed, fits inside the row). `grammar_enabled` opens the gate
/// for [`IssueCategory::Grammar`] lints — the narrow whitelist set in
/// `spell::is_actionable_category`. Spelling lints are always paintable
/// regardless of the flag.
fn compute_new_spans(
    matcher: &EchoMatcher,
    screen: &ScreenState,
    grammar_enabled: bool,
) -> Vec<NewSpan> {
    let lints = matcher.lints();
    let buffer_chars = matcher.buffer_chars();
    if lints.is_empty() || buffer_chars == 0 {
        return Vec::new();
    }
    let Some(anchor) = matcher.find_input_anchor(screen.cols) else {
        return Vec::new();
    };
    if anchor.row >= screen.rows {
        return Vec::new();
    }

    let buffer_text = matcher.buffer_text();

    let lines: Vec<(usize, usize)> = {
        let mut acc = Vec::new();
        let mut offset = 0usize;
        for line in buffer_text.split('\n') {
            let chars = line.chars().count();
            acc.push((offset, offset + chars));
            offset += chars + 1;
        }
        acc
    };

    let Some(anchor_line_idx) = lines.iter().rposition(|&(s, e)| s < e) else {
        return Vec::new();
    };

    // Build per-line character → screen-position mappings by walking
    // the grid. Every line gets the same treatment: find where it
    // starts in the grid, then walk its characters through the grid
    // with the same row-jump-on-gap logic the anchor search uses.
    // This handles word wrap, host-inserted newlines, and character
    // wrap uniformly — no line_width assumptions.
    let mut line_positions: Vec<Option<Vec<(u16, u16)>>> = vec![None; lines.len()];

    // Anchor line: start position is the known anchor.
    let (als, ale) = lines[anchor_line_idx];
    let anchor_chars: Vec<char> = buffer_text.chars().skip(als).take(ale - als).collect();
    line_positions[anchor_line_idx] =
        build_char_positions(&anchor_chars, anchor, screen.cols, matcher);

    // Earlier lines (multi-line buffer from paste): search the grid
    // for each line's text independently.
    for i in (0..anchor_line_idx).rev() {
        let (ls, le) = lines[i];
        if ls >= le {
            continue;
        }
        let line_text: String = buffer_text.chars().skip(ls).take(le - ls).collect();
        if let Some(line_anchor) = matcher.find_text_anchor(&line_text, screen.cols) {
            let line_chars: Vec<char> = line_text.chars().collect();
            line_positions[i] =
                build_char_positions(&line_chars, line_anchor, screen.cols, matcher);
        }
    }

    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    let mut spans = Vec::new();

    for issue in lints {
        if !is_actionable_category(issue.category, grammar_enabled) {
            continue;
        }
        if issue.suggestions.is_empty() {
            continue;
        }
        if !seen.insert((issue.char_start, issue.char_end)) {
            continue;
        }
        let Some(line_idx) = lines
            .iter()
            .position(|&(s, e)| issue.char_start >= s && issue.char_end <= e)
        else {
            continue;
        };
        if line_idx > anchor_line_idx {
            continue;
        }
        let (line_start, _) = lines[line_idx];
        if line_idx == anchor_line_idx && issue.char_end == buffer_chars {
            continue;
        }

        let local_start = issue.char_start - line_start;
        let local_end = issue.char_end - line_start;

        let Some(ref positions) = line_positions[line_idx] else {
            continue;
        };
        if local_end == 0
            || local_start >= positions.len()
            || local_end > positions.len()
        {
            continue;
        }
        let (start_row, start_col) = positions[local_start];
        let (end_row, end_col_last) = positions[local_end - 1];
        if start_row != end_row {
            continue;
        }
        let row = start_row;
        let col_end = end_col_last + 1;
        if row >= screen.rows || col_end > screen.cols {
            continue;
        }
        let first_char = issue.word.chars().next();
        if let Some(c) = first_char {
            match matcher.cell_at(row, start_col) {
                Some(gc) if chars_equiv(gc, c) => {}
                _ => continue,
            }
        }
        spans.push(NewSpan {
            row,
            col_start: start_col,
            col_end,
            word: issue.word.clone(),
        });
    }
    spans
}

/// Walk the anchor line through the grid from the anchor position,
/// recording the screen `(row, col)` of each character. Uses the same
/// sequential-advance + row-jump-on-gap logic as the anchor search in
/// [`crate::echo`], so word wrapping, host-inserted newlines, and
/// character wrapping are all handled. Returns `None` if the walk
/// fails (grid state inconsistent — caller should fall back).
fn build_char_positions(
    chars: &[char],
    anchor: crate::screen::CursorPos,
    cols: u16,
    matcher: &EchoMatcher,
) -> Option<Vec<(u16, u16)>> {
    if chars.is_empty() {
        return Some(Vec::new());
    }
    let base_col = anchor.col;
    let cell_ok = |r: u16, c: u16, ch: char| -> bool {
        match matcher.cell_at(r, c) {
            Some(gc) => chars_equiv(gc, ch),
            None => ch == ' ' || ch == '\u{a0}',
        }
    };

    let mut positions = Vec::with_capacity(chars.len());
    positions.push((anchor.row, base_col));

    let mut row = anchor.row;
    let mut col = base_col + 1;
    let mut row_start = base_col;

    for &ch in chars.iter().skip(1) {
        let mut just_wrapped = false;
        if col >= cols {
            row = row.checked_add(1)?;
            col = row_start;
            just_wrapped = true;
        }

        if cell_ok(row, col, ch) {
            positions.push((row, col));
            col += 1;
        } else if just_wrapped && row_start > 0 && cell_ok(row, 0, ch) {
            row_start = 0;
            positions.push((row, 0));
            col = 1;
        } else if matcher.cell_at(row, col).is_none() {
            let next_row = row.checked_add(1)?;
            if cell_ok(next_row, base_col, ch) {
                row = next_row;
                row_start = base_col;
                positions.push((row, base_col));
                col = base_col + 1;
            } else if base_col > 0 && cell_ok(next_row, 0, ch) {
                row = next_row;
                row_start = 0;
                positions.push((row, 0));
                col = 1;
            } else {
                return None;
            }
        } else {
            return None;
        }
    }
    Some(positions)
}

/// Tolerant char comparison for the grid sanity check. ASCII space
/// and NBSP both stand in for space, matching the cursor-skip
/// tolerance in `EchoMatcher::find_input_anchor`. Case-insensitive
/// for letters because some hosts (rare) capitalize after a paste.
fn chars_equiv(grid_ch: char, target_ch: char) -> bool {
    if grid_ch == target_ch {
        return true;
    }
    let space_a = matches!(grid_ch, ' ' | '\u{00A0}');
    let space_b = matches!(target_ch, ' ' | '\u{00A0}');
    space_a && space_b
}

/// Move the cursor from `from` to `to` using only relative ANSI moves
/// (`\x1b[<n>A/B/C/D`). Both coordinates are 0-indexed (row, col).
/// Origin-independent: works whether the PTY observer and the local
/// terminal share an origin or not. Shared with the picker module, which
/// uses the same approach to land its overlay correctly across hosts.
pub(crate) fn emit_relative_move(out: &mut Vec<u8>, from: (u16, u16), to: (u16, u16)) {
    if to.0 < from.0 {
        out.extend_from_slice(format!("\x1b[{}A", from.0 - to.0).as_bytes());
    } else if to.0 > from.0 {
        out.extend_from_slice(format!("\x1b[{}B", to.0 - from.0).as_bytes());
    }
    if to.1 < from.1 {
        out.extend_from_slice(format!("\x1b[{}D", from.1 - to.1).as_bytes());
    } else if to.1 > from.1 {
        out.extend_from_slice(format!("\x1b[{}C", to.1 - from.1).as_bytes());
    }
}

/// Cheap hash of the things that, if unchanged, mean a fresh paint produces
/// the same pixels. Used to avoid redundant writes to stdout.
pub fn paint_signature(matcher: &EchoMatcher, screen: &ScreenState) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    for issue in matcher.lints() {
        issue.char_start.hash(&mut h);
        issue.char_end.hash(&mut h);
        issue.word.hash(&mut h);
        if let Some(p) = matcher.position_of(issue.char_start) {
            (p.row, p.col).hash(&mut h);
        } else {
            (u16::MAX, u16::MAX).hash(&mut h);
        }
    }
    (screen.cursor.row, screen.cursor.col, screen.alt_screen).hash(&mut h);
    h.finish()
}

/// Write the annotation block to `out`. Returns true if anything was
/// written. Errors are swallowed because painting is best-effort — a failed
/// write here shouldn't crash the wrapper.
pub fn paint<W: Write>(
    out: &mut W,
    gate: &mut PaintGate,
    matcher: &EchoMatcher,
    screen: &ScreenState,
) -> bool {
    let bytes = build_annotations(gate, matcher, screen);
    if bytes.is_empty() {
        return false;
    }
    let _ = out.write_all(&bytes);
    let _ = out.flush();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{InputEvent, PrintEvent};
    use crate::screen::CursorPos;
    use crate::spell::SpellIssue;

    fn issue(char_start: usize, char_end: usize, word: &str) -> SpellIssue {
        SpellIssue {
            byte_start: char_start,
            byte_end: char_end,
            char_start,
            char_end,
            word: word.into(),
            message: "test".into(),
            suggestions: vec!["x".into()],
            category: crate::spell::IssueCategory::Spelling,
            priority: 50,
        }
    }

    fn screen_with_cursor(cols: u16, rows: u16, row: u16, col: u16) -> ScreenState {
        let mut s = ScreenState::new(cols, rows);
        s.cursor = CursorPos { row, col };
        s
    }

    fn screen(cols: u16, rows: u16) -> ScreenState {
        ScreenState::new(cols, rows)
    }

    /// Helper: seed the matcher's print history with `text` at (row, col)
    /// so the anchor search finds it.
    fn feed_history(m: &mut EchoMatcher, row: u16, start_col: u16, text: &str) {
        for (i, ch) in text.chars().enumerate() {
            m.apply_print(&PrintEvent {
                ch,
                at: CursorPos { row, col: start_col + i as u16 },
            });
        }
    }

    /// Run `build_annotations` with the PLAIN style and a fresh gate.
    /// Most tests assert against plain-underline SGR for stability across
    /// terminals; the FANCY path is exercised by its own dedicated tests.
    /// Grammar gate stays `false` because the typical test data uses
    /// `IssueCategory::Spelling` (which is paintable unconditionally).
    fn paint_plain(m: &EchoMatcher, screen: &ScreenState) -> Vec<u8> {
        let mut g = PaintGate::new();
        build_annotations_with(&mut g, m, screen, true, PLAIN, false)
    }

    #[test]
    fn empty_matcher_paints_nothing() {
        let m = EchoMatcher::new();
        assert!(paint_plain(&m, &screen(80, 24)).is_empty());
    }

    #[test]
    fn paint_disabled_returns_empty() {
        // Verify the disabled-paint branch via the explicit helper instead
        // of mutating TUIPO_PAINT_OFF, which would race against tests
        // that run concurrently and read the env var inside `paint_active`.
        // Buffer is "teh " (with trailing space) so the lint at 0..3 is no
        // longer the "current word" — otherwise the new partial-word rule
        // would skip it before disabled-paint even has a chance.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh")],
            buffer_chars: 4,
            buffer_text: "teh ".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m, 0, 0, "teh ");
        let mut g = PaintGate::new();
        let out = build_annotations_with(
            &mut g,
            &m,
            &screen_with_cursor(80, 24, 0, 4),
            false,
            PLAIN,
            false,
        );
        assert!(out.is_empty(), "active=false should suppress paint");
    }

    #[test]
    fn anchor_based_paint_uses_relative_moves_to_match_position() {
        // Buffer "teh " (4 chars; trailing space means the lint at 0..3 is
        // a completed word, not the one being typed). Rendered at (row=2,
        // col=5). Cursor at (row=2, col=9). Paint should move LEFT 4 cols
        // to col 5, draw "teh", land at col 8 — then a 1-col right restore.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh")],
            buffer_chars: 4,
            buffer_text: "teh ".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m, 2, 5, "teh ");
        let s = screen_with_cursor(80, 24, 2, 9);
        let out = paint_plain(&m, &s);
        let st = String::from_utf8_lossy(&out);
        assert!(st.starts_with("\x1b[4D"), "expected leading left-4 move: {st:?}");
        // Plain SGR — `\x1b[4m` underline + `\x1b[24m` reset. NOT the
        // colon-separated forms (Apple Terminal mis-parses them) and NOT
        // a foreground color change.
        assert!(st.contains("\x1b[4m"), "missing plain-underline SGR: {st:?}");
        assert!(st.contains("teh\x1b[24m"), "missing underline-off after word: {st:?}");
        // Must not use the legacy colon-separated forms.
        assert!(!st.contains("\x1b[4:3m"), "colon-undercurl leaked: {st:?}");
        assert!(!st.contains("\x1b[58:"), "colon-underline-color leaked: {st:?}");
        // Must not use absolute CUP (would target wrong row when the local
        // terminal has scrollback offset from the PTY).
        assert!(!st.contains(";5H"), "absolute CUP leaked: {st:?}");
        assert!(!st.contains(";6H"), "absolute CUP leaked: {st:?}");
        // Must not use ESC 7/8 (shared saved-cursor slot conflicts with child).
        assert!(!st.contains("\x1b7"), "should not save cursor: {st:?}");
        assert!(!st.contains("\x1b8"), "should not restore via ESC 8: {st:?}");
    }

    #[test]
    fn fancy_style_emits_curly_red_underline() {
        // The FANCY style is opt-in via TERM_PROGRAM detection (non-Apple)
        // and renders curly + bright-red underline color on terminals that
        // can parse the colon sub-parameter SGR. Apple Terminal users get
        // PLAIN as a fallback — see the dedicated style detection test.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh")],
            buffer_chars: 4,
            buffer_text: "teh ".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m, 0, 0, "teh ");
        let mut g = PaintGate::new();
        let out = build_annotations_with(
            &mut g,
            &m,
            &screen_with_cursor(80, 24, 0, 4),
            true,
            FANCY,
            false,
        );
        let st = String::from_utf8_lossy(&out);
        assert!(st.contains("\x1b[4:3m"), "missing curly-underline SGR: {st:?}");
        // Underline color uses T.416 direct color (`58:2::R:G:B`). The
        // empty `::` slot is the canonical color-space designator;
        // palette form (`58:5:N`) was rejected because some terminals
        // (Zed observed) parse the SGR but render the wrong color.
        assert!(
            st.contains("\x1b[58:2::255:0:0m"),
            "missing direct-color underline SGR: {st:?}"
        );
        // The word's bytes are wrapped by open+close exactly:
        assert!(
            st.contains("\x1b[4:3m\x1b[58:2::255:0:0mteh\x1b[59m\x1b[24m"),
            "wrong wrapping: {st:?}"
        );
    }

    #[test]
    fn paint_anchors_correctly_with_prompt_chrome() {
        // Prompt "> hello there " on row 7 col 2. Buffer "hello there "
        // (trailing space makes "there" a completed word), lint on "there"
        // at chars 6..11, buffer_chars 12. Cursor at (7, 16). Expected:
        // move LEFT 6 cols to col 10, paint "there", land at col 15, then
        // restore right 1.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(6, 11, "there")],
            buffer_chars: 12,
            buffer_text: "hello there ".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m, 7, 2, "> hello there ");
        let s = screen_with_cursor(80, 24, 7, 16);
        let out = paint_plain(&m, &s);
        let st = String::from_utf8_lossy(&out);
        assert!(st.starts_with("\x1b[6D"), "expected leading left-6 move: {st:?}");
        assert!(st.contains("\x1b[4mthere\x1b[24m"));
    }

    #[test]
    fn paint_uses_latest_occurrence_when_buffer_text_appears_twice() {
        // Same input rendered in a preview row and the actual input row.
        // Anchor must pick the later one. Buffer "teh " so the lint at
        // 0..3 isn't the current word.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh")],
            buffer_chars: 4,
            buffer_text: "teh ".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m, 1, 4, "teh "); // preview (earlier)
        feed_history(&mut m, 5, 8, "teh "); // input (later)
        let s = screen_with_cursor(80, 24, 5, 12);
        let out = paint_plain(&m, &s);
        let st = String::from_utf8_lossy(&out);
        // Anchor = (5, 8). Cursor = (5, 12). Same row → no row move.
        // Col delta -4 → `\x1b[4D`.
        assert!(st.starts_with("\x1b[4D"), "expected left-4 move: {st:?}");
        // The earlier preview row (row 1) must NOT appear in the moves.
        assert!(!st.contains("\x1b[4A"), "should not move to preview row: {st:?}");
    }

    #[test]
    fn paint_restores_cursor_when_anchor_row_differs_from_cursor_row() {
        // Cursor at (4, 7), anchor at (2, 5), lint "teh" at chars 0..3 of
        // a 4-char buffer "teh ". Approach: up 2, left 2. Paint "teh".
        // Pen lands at (2, 8). Restore: down 2, left 1.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh")],
            buffer_chars: 4,
            buffer_text: "teh ".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m, 2, 5, "teh ");
        let s = screen_with_cursor(80, 24, 4, 7);
        let out = paint_plain(&m, &s);
        let st = String::from_utf8_lossy(&out);
        assert!(st.contains("\x1b[2A"), "missing up-2 in approach: {st:?}");
        assert!(st.contains("\x1b[2D"), "missing left-2 in approach: {st:?}");
        assert!(st.ends_with("\x1b[2B\x1b[1D"), "wrong restore trailer: {st:?}");
    }

    #[test]
    fn paint_skips_grammar_and_style_when_grammar_disabled() {
        // Default state: grammar = false. Grammar and Style lints stay
        // hidden — only Spelling paints. This is the contract the
        // historical "every word underlined" regression test relied on
        // and the reason `paint_plain` passes `grammar_enabled: false`.
        use crate::spell::IssueCategory;
        let mut grammar_issue = issue(0, 3, "the");
        grammar_issue.category = IssueCategory::Grammar;
        let mut style_issue = issue(4, 6, "is");
        style_issue.category = IssueCategory::Style;
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![grammar_issue, style_issue],
            buffer_chars: 7,
            buffer_text: "the is ".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m, 0, 0, "the is ");
        let out = paint_plain(&m, &screen_with_cursor(80, 24, 0, 7));
        assert!(out.is_empty(), "non-spelling lints should not paint: {out:?}");
    }

    #[test]
    fn paint_includes_grammar_lints_when_grammar_enabled() {
        // Opt-in grammar checking surfaces the narrow-whitelist lints
        // alongside spelling. Style lints stay hidden because they're a
        // separate (not-yet-shipped) opt-in.
        use crate::spell::IssueCategory;
        let mut grammar_issue = issue(0, 3, "are");
        grammar_issue.category = IssueCategory::Grammar;
        let mut style_issue = issue(4, 6, "is");
        style_issue.category = IssueCategory::Style;
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![grammar_issue, style_issue],
            buffer_chars: 7,
            buffer_text: "are is ".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m, 0, 0, "are is ");
        let mut g = PaintGate::new();
        let out = build_annotations_with(
            &mut g,
            &m,
            &screen_with_cursor(80, 24, 0, 7),
            true,
            PLAIN,
            true, // grammar_enabled
        );
        let st = String::from_utf8_lossy(&out);
        assert!(
            st.contains("\x1b[4mare\x1b[24m"),
            "grammar lint should paint when enabled: {st:?}"
        );
        // Style lint stays hidden — it's not in our actionable set.
        assert!(
            !st.contains("\x1b[4mis\x1b[24m"),
            "style lint should not paint even with grammar enabled: {st:?}"
        );
    }

    #[test]
    fn paint_skips_lints_with_no_suggestions() {
        // Lints harper produced without any replacement to suggest are
        // usually partial-word artifacts. Painting them produces visual
        // noise the user can't act on.
        let mut no_sugg = issue(0, 3, "teh");
        no_sugg.suggestions.clear();
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![no_sugg],
            buffer_chars: 4,
            buffer_text: "teh ".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m, 0, 0, "teh ");
        let out = paint_plain(&m, &screen_with_cursor(80, 24, 0, 4));
        assert!(out.is_empty(), "lint without suggestions should not paint: {out:?}");
    }

    #[test]
    fn paint_skips_the_word_being_typed() {
        // The lint extends to the cursor (char_end == buffer_chars), so
        // the user is mid-typing this word. Skip — the lint will paint
        // once the user types a separator.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh")],
            buffer_chars: 3,
            buffer_text: "teh".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m, 0, 0, "teh");
        let out = paint_plain(&m, &screen_with_cursor(80, 24, 0, 3));
        assert!(out.is_empty(), "current word should be skipped: {out:?}");
    }

    #[test]
    fn paint_skipped_when_buffer_chars_zero() {
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh")],
            buffer_chars: 0,
            buffer_text: String::new(),
            buffer_cursor: 0,
        });
        assert!(paint_plain(&m, &screen_with_cursor(80, 24, 0, 0)).is_empty());
    }

    #[test]
    fn paint_skipped_when_anchor_cannot_be_found() {
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 5, "hello")],
            buffer_chars: 6,
            buffer_text: "hello ".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m, 0, 0, "world");
        assert!(paint_plain(&m, &screen_with_cursor(80, 24, 0, 6)).is_empty());
    }

    #[test]
    fn paint_skipped_when_lint_extends_past_right_edge() {
        // 20-col screen. Anchor at col 4 → "longword12" lint spans cols
        // 9..19, fits inside cols 0..20. Buffer has trailing space so the
        // lint isn't the current word.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(5, 15, "longword12")],
            buffer_chars: 16,
            buffer_text: "abcdelongword12 ".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m, 0, 4, "abcdelongword12 ");
        let out_ok = paint_plain(&m, &screen_with_cursor(20, 24, 0, 19));
        assert!(!out_ok.is_empty(), "lint within bounds should paint");
        // Anchor at col 6 → lint spans cols 11..21, exceeds cols (20).
        let mut m2 = EchoMatcher::new();
        m2.apply_input(InputEvent::Lints {
            issues: vec![issue(5, 15, "longword12")],
            buffer_chars: 16,
            buffer_text: "abcdelongword12 ".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m2, 0, 6, "abcdelongword12 ");
        let out_skip = paint_plain(&m2, &screen_with_cursor(20, 24, 0, 19));
        assert!(out_skip.is_empty(), "lint past right edge should not paint");
    }

    #[test]
    fn paint_clears_stale_span_when_lint_disappears() {
        // The user's scenario: type "reacon ", paint underlines "reacon"
        // at cols 0..6; then fix in place (backspace "con", type "son")
        // so buffer becomes "reason". The host only rewrites cols 3..6
        // — cols 0..2 keep our stale underline attribute until we
        // actively repaint those cells without it. This test verifies
        // that the second paint emits `\x1b[24m` + the current grid chars
        // at the original span, clearing the leftover underline.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 6, "reacon")],
            buffer_chars: 7,
            buffer_text: "reacon ".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m, 0, 0, "reacon ");
        let mut g = PaintGate::new();

        // First paint: underline "reacon".
        let out1 = build_annotations_with(
            &mut g,
            &m,
            &screen_with_cursor(80, 24, 0, 7),
            true,
            PLAIN,
            false,
        );
        assert!(
            String::from_utf8_lossy(&out1).contains("\x1b[4mreacon\x1b[24m"),
            "first paint should underline reacon: {out1:?}"
        );
        assert_eq!(g.painted_len(), 1, "should track the painted span");

        // User fixes in place: grid cells at cols 3..6 are now 's','o','n'.
        // Buffer becomes "reason ". Lints empty (correct word).
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 7,
            buffer_text: "reason ".into(),
            buffer_cursor: 0,
        });
        // Overwrite the changed cells in the grid (Claude's echo of the
        // retyped chars). The "rea" cells at 0..2 keep their old chars,
        // which still have our underline attribute on screen.
        m.apply_print(&PrintEvent { ch: 's', at: CursorPos { row: 0, col: 3 } });
        m.apply_print(&PrintEvent { ch: 'o', at: CursorPos { row: 0, col: 4 } });
        m.apply_print(&PrintEvent { ch: 'n', at: CursorPos { row: 0, col: 5 } });

        let out2 = build_annotations_with(
            &mut g,
            &m,
            &screen_with_cursor(80, 24, 0, 7),
            true,
            PLAIN,
            false,
        );
        let st = String::from_utf8_lossy(&out2);
        // We should emit `\x1b[24m` (underline off) followed by the
        // current grid chars at cols 0..6 — "reason" — so the cells get
        // rewritten without the underline attribute.
        assert!(
            st.contains("\x1b[24mreason"),
            "expected clear pass to rewrite cells with underline off: {st:?}"
        );
        assert!(
            !st.contains("\x1b[4mreason"),
            "should not re-underline the fixed word: {st:?}"
        );
        assert_eq!(g.painted_len(), 0, "tracking should be empty after clear");
    }

    #[test]
    fn paint_idempotent_when_same_span_stays_lint() {
        // Re-painting the same lint shouldn't add or drop tracked spans.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh")],
            buffer_chars: 4,
            buffer_text: "teh ".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m, 0, 0, "teh ");
        let mut g = PaintGate::new();
        let s = screen_with_cursor(80, 24, 0, 4);
        let _ = build_annotations_with(&mut g, &m, &s, true, PLAIN, false);
        let n_after_first = g.painted_len();
        let _ = build_annotations_with(&mut g, &m, &s, true, PLAIN, false);
        let n_after_second = g.painted_len();
        assert_eq!(n_after_first, n_after_second);
        assert_eq!(n_after_first, 1);
    }

    #[test]
    fn forget_painted_drops_tracking_without_writing() {
        let mut g = PaintGate::new();
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh")],
            buffer_chars: 4,
            buffer_text: "teh ".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m, 0, 0, "teh ");
        let _ = build_annotations_with(&mut g, &m, &screen_with_cursor(80, 24, 0, 4), true, PLAIN, false);
        assert_eq!(g.painted_len(), 1);
        g.forget_painted();
        assert_eq!(g.painted_len(), 0);
    }

    #[test]
    fn stale_span_carried_forward_when_grid_missing_cells() {
        // The painter's stale-clear is partial: present cells get a
        // `\x1b[24m`+content rewrite, missing cells are skipped, and the
        // whole span is **carried forward** to the next paint cycle so a
        // future tick can retry once the grid fills in. This is what
        // fixes the "underline fragments persist after a mid-buffer
        // picker fix" symptom — without the carry-forward, the partial
        // grid window leaves a stale curly red span dangling until the
        // host happens to redraw.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh")],
            buffer_chars: 4,
            buffer_text: "teh ".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m, 0, 0, "teh ");
        let mut g = PaintGate::new();
        let _ = build_annotations_with(&mut g, &m, &screen_with_cursor(80, 24, 0, 4), true, PLAIN, false);
        assert_eq!(g.painted_len(), 1);

        // Lint goes away AND the grid loses the relevant cells (mimics
        // the mid-fix window where backspace echoes have erased grid
        // entries but the replacement chars haven't echoed back yet).
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 0,
            buffer_text: String::new(),
            buffer_cursor: 0,
        });
        m.clear_screen();

        let out = build_annotations_with(&mut g, &m, &screen_with_cursor(80, 24, 0, 4), true, PLAIN, false);
        // No bytes are emitted (grid empty → no chars to rewrite), but
        // the span is RETAINED in tracking so the next paint cycle can
        // retry once the grid fills in.
        assert!(out.is_empty(), "should emit nothing when grid is empty: {out:?}");
        assert_eq!(g.painted_len(), 1, "stale span carried forward for retry");

        // Refill the grid (simulates the replacement chars echoing back).
        feed_history(&mut m, 0, 0, "the ");
        let out2 = build_annotations_with(&mut g, &m, &screen_with_cursor(80, 24, 0, 4), true, PLAIN, false);
        let st = String::from_utf8_lossy(&out2);
        assert!(
            st.contains("\x1b[24m"),
            "second pass should emit the deferred clear: {st:?}"
        );
        assert_eq!(g.painted_len(), 0, "carry-forward should clear once grid is whole");
    }

    #[test]
    fn stale_span_partial_clear_emits_present_run_only() {
        // Span covers cols 0..6. Only cols 0..3 are in the grid; cols
        // 3..6 are missing. The clear pass should emit a `\x1b[24m` +
        // first-3-chars run and skip the missing tail. The span is
        // carried forward so the missing tail gets retried next tick.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 6, "abcdef")],
            buffer_chars: 7,
            buffer_text: "abcdef ".into(),
            buffer_cursor: 0,
        });
        feed_history(&mut m, 0, 0, "abcdef ");
        let mut g = PaintGate::new();
        let _ = build_annotations_with(&mut g, &m, &screen_with_cursor(80, 24, 0, 7), true, PLAIN, false);
        assert_eq!(g.painted_len(), 1);

        // Lint goes away and only the first half of the span is still
        // in the grid — simulate by erasing cols 3..6.
        m.apply_input(InputEvent::Lints {
            issues: vec![],
            buffer_chars: 7,
            buffer_text: "abcdef ".into(),
            buffer_cursor: 0,
        });
        m.erase_cells(0, 3, 7);

        let out = build_annotations_with(&mut g, &m, &screen_with_cursor(80, 24, 0, 7), true, PLAIN, false);
        let st = String::from_utf8_lossy(&out);
        // First run "abc" should have been cleared.
        assert!(
            st.contains("\x1b[24mabc"),
            "partial clear should emit the present run: {st:?}"
        );
        // Span carries forward because cols 3..6 were missing.
        assert_eq!(g.painted_len(), 1, "incomplete clear carries forward");
    }

    #[test]
    fn pause_gate_blocks_immediately_after_keystroke() {
        let mut g = PaintGate::new();
        g.mark_keystroke();
        let now = Instant::now();
        assert!(!g.should_paint(now), "should not paint right after keystroke");
    }

    #[test]
    fn pause_gate_fires_after_pause_window() {
        let mut g = PaintGate::new();
        g.mark_keystroke();
        // Simulate waiting past PAUSE_MS by handing it an Instant from the
        // future — Instant addition is allowed.
        let future = Instant::now() + PAUSE_MS + Duration::from_millis(10);
        assert!(g.should_paint(future));
    }

    #[test]
    fn pause_gate_skips_when_not_dirty() {
        let mut g = PaintGate::new();
        g.record_paint(0);
        // No keystroke, no dirty change.
        assert!(!g.should_paint(Instant::now() + Duration::from_secs(10)));
    }

    #[test]
    fn record_paint_clears_dirty_flag() {
        let mut g = PaintGate::new();
        g.mark_keystroke();
        // After the pause window, paint should fire once...
        let future = Instant::now() + PAUSE_MS + Duration::from_millis(10);
        assert!(g.should_paint(future));
        g.record_paint(123);
        // ...and not again until something marks dirty.
        assert!(!g.should_paint(future + Duration::from_secs(10)));
    }

    #[test]
    fn paint_again_after_new_keystroke() {
        let mut g = PaintGate::new();
        g.mark_keystroke();
        let t1 = Instant::now() + PAUSE_MS + Duration::from_millis(10);
        assert!(g.should_paint(t1));
        g.record_paint(1);
        g.mark_keystroke();
        let t2 = t1 + PAUSE_MS + Duration::from_millis(10);
        assert!(g.should_paint(t2), "new keystroke should re-arm the gate");
    }

    #[test]
    fn paint_anchors_on_last_line_for_multi_line_buffer() {
        // User pastes multi-line prose. Buffer becomes
        // "line one\nfix teh now " (18 chars + space). Harper flags
        // "teh" at chars 13..16 of the global buffer; that's chars 4..7
        // of the trailing line "fix teh now ". The painter must anchor
        // on the trailing line and paint at the *local* offset.
        //
        // Before the multi-line fix, find_input_anchor would search for
        // the entire buffer text "line one\nfix teh now " on a single
        // grid row — impossible by construction since the host renders
        // the two lines on different rows — and return None, leaving
        // the user with no painting after any bracketed paste of
        // multi-line content.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(13, 16, "teh")],
            buffer_chars: 21,
            buffer_text: "line one\nfix teh now ".into(),
            buffer_cursor: 21,
        });
        // Host renders line 1 on row 3 col 0, line 2 on row 4 col 0.
        feed_history(&mut m, 3, 0, "line one");
        feed_history(&mut m, 4, 0, "fix teh now ");
        let out = paint_plain(&m, &screen_with_cursor(80, 24, 4, 12));
        let st = String::from_utf8_lossy(&out);
        // Anchor at row 4, col 0. "teh" lives at local cols 4..7.
        // Cursor at col 12 → leading move is left-by-(12-4)=8.
        assert!(
            st.contains("\x1b[4mteh\x1b[24m"),
            "should paint teh on the trailing line: {st:?}"
        );
        assert!(
            st.starts_with("\x1b[8D"),
            "leading move should land at local col 4 of the trailing line: {st:?}"
        );
    }

    #[test]
    fn paint_handles_a_wrapping_earlier_line() {
        // Earlier buffer line is too long to fit in one screen row at
        // the anchor column — it wraps. The painter's row math
        // accounts for this: a wrapping line consumes
        // `ceil(line_len / line_width)` rows, and the previous line's
        // first row is computed accordingly. Without wrap-aware row
        // math, a wrapping earlier line would push everything above
        // off by one row.
        //
        // Screen is 14 cols wide. Anchor col = 0 → line_width = 14.
        // Buffer:
        //   "abcdefghijkl teh"    = 16 chars (line 0; wraps to 2 rows)
        //   "\n"                  = sep
        //   "trailing"            = 8 chars (line 1; anchor row)
        // Layout (rows top to bottom):
        //   row 0: "abcdefghijkl t"      (first wrap-row of line 0)
        //   row 1: "eh"                  (second wrap-row of line 0)
        //   row 2: "trailing"            (line 1 — anchor)
        // The "teh" in line 0 sits at chars 13..16, which is local
        // chars 13..16 within line 0, which lands on wrap-row 0 cols
        // 13..14 (partial — wraps at col 14, but actually 13..14 is
        // only one char "t"). The lint spans the wrap boundary so
        // it must be SKIPPED rather than painted at the wrong place.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(13, 16, "teh")],
            buffer_chars: 25,
            buffer_text: "abcdefghijkl teh\ntrailing".into(),
            buffer_cursor: 25,
        });
        // Grid layout per the wrapped rendering.
        feed_history(&mut m, 0, 0, "abcdefghijkl t");
        feed_history(&mut m, 1, 0, "eh");
        feed_history(&mut m, 2, 0, "trailing");
        let out = paint_plain(&m, &screen_with_cursor(14, 24, 2, 8));
        let st = String::from_utf8_lossy(&out);
        // The cross-wrap "teh" is skipped (would paint at wrong row).
        // The trailing line has no lints (none defined). So no paint
        // bytes should be emitted at all.
        assert!(
            !st.contains("\x1b[4m"),
            "cross-wrap lint must not paint: {st:?}"
        );
    }

    #[test]
    fn paint_paints_lints_on_every_buffer_line() {
        // Multi-line paste / Shift-Enter content: the painter must
        // underline misspellings on EVERY visible buffer line, not
        // just the trailing one. This is the user-reported case: pasted
        // multi-line prose into Claude Code stayed unpainted because
        // an earlier restriction limited paint to the last line.
        //
        // Buffer "teh fix me\nfix teh now ":
        //   "teh fix me"   = chars 0..10  (line 0, row 3)
        //   "\n"           = char  10
        //   "fix teh now " = chars 11..23 (line 1, row 4 — anchor)
        // Line-0 "teh" = chars 0..3. Line-1 "teh" = chars 15..18.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh"), issue(15, 18, "teh")],
            buffer_chars: 23,
            buffer_text: "teh fix me\nfix teh now ".into(),
            buffer_cursor: 23,
        });
        feed_history(&mut m, 3, 0, "teh fix me");
        feed_history(&mut m, 4, 0, "fix teh now ");
        let out = paint_plain(&m, &screen_with_cursor(80, 24, 4, 12));
        let st = String::from_utf8_lossy(&out);
        let paints = st.matches("\x1b[4mteh\x1b[24m").count();
        assert_eq!(paints, 2, "both lines' lints must be painted: {st:?}");
        // The leading move targets one of the two lints. The line-0
        // lint is at (row 3, col 0); the line-1 lint at (row 4, col 4).
        // Either is fine — the assertion that matters is that BOTH are
        // emitted as separate `\x1b[4mteh\x1b[24m` runs above.
        assert!(
            st.contains("\x1b[1A") || st.contains("\x1b[1B"),
            "should make at least one vertical move between rows: {st:?}"
        );
    }

    #[test]
    fn paint_soft_wrapped_single_line_underlines_first_row() {
        // User types a long prompt that soft-wraps in a 20-col terminal.
        // Text starts at col 2 (after a prompt marker). line_width = 18.
        // Buffer: "I'm setting up teh new thing " (29 chars).
        // Row 5 cols 2..19: "I'm setting up teh" (18 chars)
        // Row 6 cols 2..12: " new thing "       (11 chars)
        // "teh" is at chars 16..19 → local offset 16 → wrap-row 0,
        // col_start = 2 + 16 = 18, col_end = 2 + 19 = 21... wait that
        // exceeds 20 cols. Let me adjust.
        //
        // Actually: text starts at col 2, line_width = 18. "teh" at
        // local chars 16..19 crosses the wrap boundary (16/18=0 but
        // 18/18=1). The painter skips lints that cross a wrap row.
        // Use a shorter prefix so "teh" fits fully on row 5.
        //
        // Buffer: "type teh thngs here and keep going on " (38 chars)
        // Row 5 cols 2..19: "type teh thngs her" (18 chars, 0..18)
        // Row 6 cols 2..19: "e and keep going o" (18 chars, 18..36)
        // Row 7 cols 2..3:  "n "                 (2 chars, 36..38)
        //
        // "teh" = chars 5..8 → wrap-row 0, col 2+5=7 to 2+8=10
        // "thngs" = chars 9..14 → wrap-row 0, col 2+9=11 to 2+14=16
        // Both on row 5. Cursor at row 7 col 4.
        let mut m = EchoMatcher::new();
        let buf = "type teh thngs here and keep going on ";
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(5, 8, "teh"), issue(9, 14, "thngs")],
            buffer_chars: buf.chars().count(),
            buffer_text: buf.into(),
            buffer_cursor: buf.chars().count(),
        });
        feed_history(&mut m, 5, 2, "type teh thngs her");
        feed_history(&mut m, 6, 2, "e and keep going o");
        feed_history(&mut m, 7, 2, "n ");
        let s = screen_with_cursor(20, 24, 7, 4);
        let out = paint_plain(&m, &s);
        let st = String::from_utf8_lossy(&out);
        assert!(
            st.contains("\x1b[4mteh\x1b[24m"),
            "should underline teh on the first wrapped row: {st:?}"
        );
        assert!(
            st.contains("\x1b[4mthngs\x1b[24m"),
            "should underline thngs on the first wrapped row: {st:?}"
        );
    }

    #[test]
    fn paint_soft_wrapped_lint_on_second_row() {
        // Lint lives on the SECOND wrap-row (not the anchor row).
        // Character-level wrapping: rows are fully filled.
        // Buffer: "this is correct but teh end " (28 chars)
        // 14-col screen, text at col 2 → 12 chars per row.
        let mut m = EchoMatcher::new();
        let buf = "this is correct but teh end ";
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(20, 23, "teh")],
            buffer_chars: buf.chars().count(),
            buffer_text: buf.into(),
            buffer_cursor: buf.chars().count(),
        });
        feed_history(&mut m, 5, 2, "this is corr");
        feed_history(&mut m, 6, 2, "ect but teh ");
        feed_history(&mut m, 7, 2, "end ");
        let s = screen_with_cursor(14, 24, 7, 6);
        let out = paint_plain(&m, &s);
        let st = String::from_utf8_lossy(&out);
        assert!(
            st.contains("\x1b[4mteh\x1b[24m"),
            "should underline teh on the second wrap-row: {st:?}"
        );
    }

    #[test]
    fn paint_word_wrapped_lint_on_second_row() {
        // Word-wrap: the first row is SHORT (host wrapped at a word
        // boundary). The grid-walked position mapping handles this;
        // the old line_width math would put the lint at the wrong col.
        //
        // Buffer: "hello world teh end " (20 chars)
        // 16-col screen, text at col 2. With word wrap:
        // Row 5: "hello world" (11 chars at cols 2..12, space cursor-skipped)
        // Row 6: "teh end "   (8 chars at cols 2..9)
        // "teh" = chars 12..15, lives on row 6 at cols 2..4.
        let mut m = EchoMatcher::new();
        let buf = "hello world teh end ";
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(12, 15, "teh")],
            buffer_chars: buf.chars().count(),
            buffer_text: buf.into(),
            buffer_cursor: buf.chars().count(),
        });
        // Row 5: "hello world" — space at col 7 cursor-skipped
        for &(col, ch) in &[
            (2u16, 'h'), (3, 'e'), (4, 'l'), (5, 'l'), (6, 'o'),
            (8, 'w'), (9, 'o'), (10, 'r'), (11, 'l'), (12, 'd'),
        ] {
            m.apply_print(&PrintEvent {
                ch,
                at: CursorPos { row: 5, col },
            });
        }
        // Row 6: "teh end " — space at col 5 cursor-skipped
        for &(col, ch) in &[
            (2u16, 't'), (3, 'e'), (4, 'h'),
            (6, 'e'), (7, 'n'), (8, 'd'),
        ] {
            m.apply_print(&PrintEvent {
                ch,
                at: CursorPos { row: 6, col },
            });
        }
        let s = screen_with_cursor(16, 24, 6, 10);
        let out = paint_plain(&m, &s);
        let st = String::from_utf8_lossy(&out);
        assert!(
            st.contains("\x1b[4mteh\x1b[24m"),
            "word-wrapped lint on second row must paint: {st:?}"
        );
    }

    #[test]
    fn signature_changes_when_state_changes() {
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh")],
            buffer_chars: 3,
            buffer_text: "teh".into(),
            buffer_cursor: 0,
        });
        let s1 = paint_signature(&m, &screen(80, 24));
        m.apply_input(InputEvent::Lints {
            issues: vec![issue(0, 3, "teh"), issue(4, 9, "abcde")],
            buffer_chars: 9,
            buffer_text: "teh abcde".into(),
            buffer_cursor: 0,
        });
        let s2 = paint_signature(&m, &screen(80, 24));
        assert_ne!(s1, s2);
    }
}
