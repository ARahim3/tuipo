//! Status-row renderer.
//!
//! Owns the absolute bottom row of the terminal when explicitly enabled
//! via `status_row = true` (or `TUIPO_STATUS=on`). Two states:
//!
//! - Issues present: "tuipo · N issues · Tab to fix"
//! - No issues: empty (we erase the row).
//!
//! Each draw is bracketed by `ESC 7` / `ESC 8` so the child's cursor and
//! attributes are untouched. The child's bottom-row content is collateral
//! damage during the brief moments we paint — future phase 7c can solve
//! that by actually reserving the row via PTY size.
//!
//! **No picker UI here.** An earlier prototype rendered the picker in
//! this row, but it now lives inline next to the misspelling (see
//! `picker.rs`). Rendering both would draw the same options twice; and
//! once drawn the bottom row never cleared itself if the picker closed
//! without a fresh status pass, leaving stale text the user could never
//! interact with. Killing the picker path here is what fixes that.

use crate::echo::EchoMatcher;
use crate::screen::ScreenState;

/// Compose the status-row ANSI bytes. Empty result means "nothing to draw".
///
/// **Idle behavior**: we DON'T show an issue count by default — too many
/// hosts (notably Claude Code) redraw their own bottom row aggressively
/// and would race with our paint. The status row is drawn only when
/// `TUIPO_STATUS=on` (or `status_row = true` in the config) is set.
pub fn build_status(matcher: &EchoMatcher, screen: &ScreenState) -> Vec<u8> {
    if !crate::paint::paint_active(screen) {
        return Vec::new();
    }
    if !crate::config::get().status_row_enabled() {
        return Vec::new();
    }
    let text = format_count(matcher);
    if text.is_empty() {
        return Vec::new();
    }

    let row = screen.rows; // 1-indexed last row
    let mut out: Vec<u8> = Vec::with_capacity(64 + text.len());
    out.extend_from_slice(b"\x1b7"); // save cursor + attrs
    out.extend_from_slice(format!("\x1b[{row};1H").as_bytes()); // CUP to last row, col 1
    out.extend_from_slice(b"\x1b[2K"); // erase entire line
    // Dim cyan reads as "ambient UI" and contrasts nicely with most prompts.
    out.extend_from_slice(b"\x1b[2;36m");
    out.extend_from_slice(truncate_to_width(&text, screen.cols as usize).as_bytes());
    out.extend_from_slice(b"\x1b[0m");
    out.extend_from_slice(b"\x1b8"); // restore
    out
}

fn format_count(matcher: &EchoMatcher) -> String {
    // Count only the lints we'd actually surface — otherwise the row
    // would say "5 issues" while the painter shows zero underlines and
    // the picker engages on none of them. Filter mirrors what the
    // painter and picker use.
    let grammar_enabled = crate::config::get().grammar_enabled();
    let unique_words: std::collections::BTreeSet<&str> = matcher
        .lints()
        .iter()
        .filter(|i| crate::spell::is_actionable_category(i.category, grammar_enabled))
        .map(|i| i.word.as_str())
        .collect();
    let n = unique_words.len();
    if n == 0 {
        return String::new();
    }
    let plural = if n == 1 { "" } else { "s" };
    format!("tuipo · {n} issue{plural} · Tab to fix")
}

fn truncate_to_width(s: &str, max_cols: usize) -> String {
    // Conservative: count chars (most terminals render 1 cell per char for
    // ASCII; double-width chars get clipped early but that's fine).
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i + 1 > max_cols {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::InputEvent;
    use crate::spell::{IssueCategory, SpellIssue};

    fn issue(word: &str) -> SpellIssue {
        SpellIssue {
            byte_start: 0,
            byte_end: word.len(),
            char_start: 0,
            char_end: word.chars().count(),
            word: word.into(),
            message: "test".into(),
            suggestions: vec![format!("{word}-fixed")],
            category: IssueCategory::Spelling,
            priority: 50,
        }
    }

    fn term(cols: u16, rows: u16) -> ScreenState {
        ScreenState::new(cols, rows)
    }

    #[test]
    fn no_idle_status_by_default() {
        // The default UX is "show paint underlines, not a chrome status
        // row" — most hosts redraw their bottom row and would fight us.
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue("teh")],
            buffer_chars: 3,
            buffer_text: "teh".into(),
            buffer_cursor: 0,
        });
        assert!(build_status(&m, &term(80, 24)).is_empty());
    }

    #[test]
    fn no_status_when_no_issues() {
        let m = EchoMatcher::new();
        assert!(build_status(&m, &term(80, 24)).is_empty());
    }

    #[test]
    fn opt_in_status_shows_count() {
        unsafe { std::env::set_var("TUIPO_STATUS", "on") };
        let mut m = EchoMatcher::new();
        m.apply_input(InputEvent::Lints {
            issues: vec![issue("teh"), issue("paragprah")],
            buffer_chars: 13,
            buffer_text: "teh paragprah".into(),
            buffer_cursor: 0,
        });
        let bytes = build_status(&m, &term(80, 24));
        unsafe { std::env::remove_var("TUIPO_STATUS") };
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("2 issues"), "expected `2 issues`, got: {s:?}");
    }
}
