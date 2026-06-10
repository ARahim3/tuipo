//! Tab-fix: applies a suggestion for a misspelling.
//!
//! Two cursor regimes:
//!
//! - **Cursor at end-of-buffer** (the classic Tab-fix path).
//!   Backspace from the cursor over `[issue.char_start, cursor)`, then
//!   retype `suggestion + trailing_text` so anything typed after the
//!   misspelling is preserved and the cursor lands back at end-of-buffer.
//! - **Cursor inside or past the misspelling** (the picker's mid-buffer
//!   path). The user moved their cursor into a flagged word via arrow
//!   keys and asked us to fix it without disturbing the rest of the
//!   line. We inject arrow keys to position the cursor at `char_end`,
//!   backspace just the misspelling, and type only the suggestion. The
//!   trailing text is never deleted, so it never needs to be retyped.
//!
//! Why two regimes: backspaces delete chars *before* the cursor, so we
//! must always position the cursor at the right edge of what we want
//! gone. The two cases differ only in how we got there.

use crate::input::InputState;
use crate::spell::SpellIssue;

/// Bytes to inject into the PTY's stdin (and into the local buffer) to
/// apply a fix. Order: `left_moves` (arrow-left CSIs), then `right_moves`
/// (arrow-right CSIs), then `backspaces`, then `replacement_text`. At
/// most one of `left_moves` / `right_moves` is non-zero per fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    /// Arrow-left CSIs to send before backspacing. Non-zero when the
    /// cursor was past `issue.char_end` (e.g. user moved past the word
    /// before asking to fix it) and we need to move back to align with
    /// the right edge of the misspelling.
    pub left_moves: usize,
    /// Arrow-right CSIs to send before backspacing. Non-zero when the
    /// cursor sits *inside* the misspelling — we walk forward to
    /// `issue.char_end` so the backspaces consume exactly the misspelled
    /// chars.
    pub right_moves: usize,
    /// Backspace presses to delete the misspelling.
    pub backspaces: usize,
    /// What to type after the deletion lands. For end-of-buffer fixes
    /// this includes the trailing text (preserved verbatim); for
    /// mid-buffer fixes it's just the suggestion.
    pub replacement_text: String,
    /// The issue this fix targets — useful for debug logs / future picker UI.
    pub issue_word: String,
}

pub fn try_tab_fix(state: &InputState) -> Option<Fix> {
    let issue = find_fixable_issue(state)?;
    let suggestion = issue.suggestions.first()?;
    if suggestion.is_empty() || *suggestion == issue.word {
        return None;
    }
    build_fix(state, issue, suggestion)
}

/// Find a fixable issue at/before the cursor with only whitespace between.
/// Returns the most recently typed one (highest `char_start`) so Tab fixes
/// what the user just typed, not something five words back.
pub fn find_fixable_issue(state: &InputState) -> Option<&SpellIssue> {
    let buffer = state.buffer();
    let text = buffer.text();
    if buffer.cursor() != text.len() {
        return None;
    }
    let cursor_char_count = text.chars().count();
    pick_issue(state.issues(), text, cursor_char_count)
}

/// Up to `max` deduplicated suggestion strings for an issue, in priority
/// order. harper may return duplicates (e.g. spelling + typo lints for the
/// same word); we collapse them here.
#[allow(dead_code)]
pub fn top_suggestions(issue: &SpellIssue, max: usize) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(max);
    for s in &issue.suggestions {
        if s.is_empty() || *s == issue.word {
            continue;
        }
        if seen.insert(s.clone()) {
            out.push(s.clone());
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

/// Build a fix that replaces `issue` with `suggestion` in the current
/// buffer. Handles both end-of-buffer (legacy Tab-fix) and mid-buffer
/// (picker) cursor positions — see the module-level docs for the regime
/// split. Returns None if the issue's span doesn't fit inside the
/// current buffer (stale lint).
pub fn build_fix(state: &InputState, issue: &SpellIssue, suggestion: &str) -> Option<Fix> {
    let text = state.buffer().text();
    let text_chars = text.chars().count();
    if issue.char_end > text_chars {
        return None;
    }
    // Multi-line buffer (bracketed-paste content). Tab-fix only operates
    // on the trailing line — fixing an earlier-line lint would either
    // backspace over a `\n` (mid-buffer path) or retype trailing text
    // that contains `\n` (end-of-buffer path). Either way, the injected
    // `\n` triggers `boundary_reset` in the buffer state machine — out
    // of paste mode it isn't content, it's a submit — and corrupts the
    // input. Mirror the painter's "only paint the last line" policy
    // (see `paint::compute_new_spans`) so Tab-fix only operates on
    // lints the user can actually see underlined.
    let last_line_offset = last_line_char_offset(text);
    if issue.char_start < last_line_offset {
        return None;
    }
    let cursor_chars = char_offset_of_byte(text, state.buffer().cursor());
    if cursor_chars == text_chars {
        // Cursor at end-of-buffer: classic Tab-fix. Backspace all the way
        // back to `char_start`, then retype `suggestion + trailing` so
        // the trailing text is preserved and the cursor lands at end.
        let backspaces = text_chars - issue.char_start;
        let trailing: String = text.chars().skip(issue.char_end).collect();
        let replacement_text = format!("{suggestion}{trailing}");
        return Some(Fix {
            left_moves: 0,
            right_moves: 0,
            backspaces,
            replacement_text,
            issue_word: issue.word.clone(),
        });
    }
    // Cursor inside the buffer (picker engaged after the user moved the
    // cursor into a finished word via arrow keys). Align the cursor to
    // `char_end` with arrow keys, backspace exactly the misspelling, and
    // type only the suggestion. Trailing text is untouched.
    let (left_moves, right_moves) = if cursor_chars < issue.char_end {
        (0, issue.char_end - cursor_chars)
    } else {
        (cursor_chars - issue.char_end, 0)
    };
    let backspaces = issue.char_end - issue.char_start;
    Some(Fix {
        left_moves,
        right_moves,
        backspaces,
        replacement_text: suggestion.to_string(),
        issue_word: issue.word.clone(),
    })
}

/// Byte cursor → char cursor within `text`. Same logic the stdin pump
/// uses (`char_cursor_in`), inlined here so callers of `build_fix`
/// don't need to compute it themselves.
fn char_offset_of_byte(text: &str, byte_cursor: usize) -> usize {
    let clamped = byte_cursor.min(text.len());
    text[..clamped].chars().count()
}

/// Char offset of the trailing line of `text`. Duplicates the matcher's
/// `EchoMatcher::last_line_char_offset` so `build_fix` doesn't have to
/// take the matcher as a dependency — the buffer's own text is the
/// source of truth for which line is which.
fn last_line_char_offset(text: &str) -> usize {
    match text.rfind('\n') {
        Some(byte_idx) => text[..=byte_idx].chars().count(),
        None => 0,
    }
}

fn pick_issue<'a>(
    issues: &'a [SpellIssue],
    _text: &str,
    cursor_char_count: usize,
) -> Option<&'a SpellIssue> {
    // Pick the most recently typed misspelling (highest char_start). We
    // used to require whitespace-only between the issue and the cursor,
    // but `build_fix` already preserves anything after the issue by
    // re-typing it after the suggestion, so the restriction wasn't doing
    // useful work — it just blocked Tab in the common case of "I typed
    // a whole sentence with typos".
    let mut best: Option<&SpellIssue> = None;
    for issue in issues {
        if issue.char_end > cursor_char_count {
            continue;
        }
        match best {
            Some(b) if b.char_start >= issue.char_start => {}
            _ => best = Some(issue),
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::FeedOutcome;
    use crate::input::InputState;

    fn make_state(input: &[u8]) -> InputState {
        let mut s = InputState::new();
        let out = s.feed_bytes(input);
        // Ensure feeding worked
        let _ = out;
        s
    }

    #[test]
    fn fix_at_cursor_no_trailing_space() {
        let state = make_state(b"teh");
        let fix = try_tab_fix(&state).expect("should fix `teh`");
        assert_eq!(fix.issue_word, "teh");
        assert_eq!(fix.backspaces, 3);
        assert_eq!(fix.replacement_text, "the");
        assert_eq!(fix.left_moves, 0);
        assert_eq!(fix.right_moves, 0);
    }

    #[test]
    fn fix_with_trailing_space_preserves_it() {
        let state = make_state(b"teh ");
        let fix = try_tab_fix(&state).expect("should fix `teh ` with trailing space");
        assert_eq!(fix.backspaces, 4, "must delete teh + the space");
        // Top suggestion comes from harper; the prefix is the corrected word
        // and the suffix is the preserved whitespace.
        assert!(
            fix.replacement_text.starts_with("the"),
            "replacement should start with `the`, got: {:?}",
            fix.replacement_text,
        );
        assert!(
            fix.replacement_text.ends_with(' '),
            "replacement should end with the preserved space, got: {:?}",
            fix.replacement_text,
        );
        assert_eq!(fix.left_moves, 0);
        assert_eq!(fix.right_moves, 0);
    }

    #[test]
    fn build_fix_mid_buffer_emits_right_moves_and_minimal_backspaces() {
        // The picker scenario: user typed a sentence, then moved the
        // cursor INTO the misspelling. Build a stub issue at chars 6..9
        // ("teh") with cursor at 7 (between 't' and 'e'). We expect 2
        // right-moves (to land cursor at char_end=9), 3 backspaces (to
        // delete "teh"), and replacement = just the suggestion (no
        // trailing-text retype — the trailing text is left untouched).
        let mut s = InputState::new();
        s.feed_bytes(b"write teh paragprah for the");
        // Park the cursor at char 7 (inside "teh"). Buffer is ASCII so
        // char offset == byte offset.
        for _ in 0.."write teh paragprah for the".len() - 7 {
            s.feed_bytes(&[0x1B, b'[', b'D']);
        }
        let teh = s
            .issues()
            .iter()
            .find(|i| i.word == "teh")
            .cloned()
            .expect("expected harper to flag `teh`");
        let fix = build_fix(&s, &teh, "the").expect("fix must build");
        assert_eq!(fix.right_moves, 2, "should walk cursor to char_end (7→9)");
        assert_eq!(fix.left_moves, 0);
        assert_eq!(fix.backspaces, 3, "delete only the 3 chars of `teh`");
        assert_eq!(fix.replacement_text, "the", "type only the suggestion");
    }

    /// Regression for the picker-engaged Enter bug. Before the cursor-aware
    /// fix logic, build_fix used `text.chars().count()` as the cursor
    /// position and computed backspaces from the *length* of the buffer
    /// (not the actual cursor), so a mid-buffer Enter would backspace far
    /// too many chars and re-type the trailing text, producing "the
    /// paragraph for the paragraph for the" instead of "write the
    /// paragprah for the". This test reconstructs the original keystroke
    /// sequence and asserts the post-fix buffer matches what the user
    /// would expect.
    #[test]
    fn mid_buffer_fix_preserves_surrounding_text() {
        let mut s = InputState::new();
        s.feed_bytes(b"write teh paragprah for the");
        // Move the cursor back from position 27 to position 9 (just after
        // "teh") by sending 18 left-arrow CSIs.
        for _ in 0..18 {
            s.feed_bytes(&[0x1B, b'[', b'D']);
        }
        let teh = s
            .issues()
            .iter()
            .find(|i| i.word == "teh")
            .cloned()
            .expect("expected harper to flag `teh`");
        let fix = build_fix(&s, &teh, "the").expect("fix must build");
        // Cursor was at char 9 == char_end → no arrow moves needed.
        assert_eq!(fix.left_moves, 0);
        assert_eq!(fix.right_moves, 0);
        // Apply the fix's operations to the same state and verify the
        // buffer ends in the correct shape. This is the contract
        // apply_fix relies on: feeding the recorded byte sequence yields
        // the right text without touching anything outside the
        // misspelling's span.
        for _ in 0..fix.left_moves {
            s.feed_bytes(b"\x1b[D");
        }
        for _ in 0..fix.right_moves {
            s.feed_bytes(b"\x1b[C");
        }
        for _ in 0..fix.backspaces {
            s.feed_bytes(&[0x08]);
        }
        s.feed_bytes(fix.replacement_text.as_bytes());
        assert_eq!(s.buffer().text(), "write the paragprah for the");
    }

    #[test]
    fn build_fix_mid_buffer_emits_left_moves_when_cursor_past_word() {
        // Cursor is just past the misspelling but not at end-of-buffer
        // (e.g. user typed "teh world" and moved cursor back to between
        // the space and 'w'). Expect left_moves to align to char_end.
        let mut s = InputState::new();
        s.feed_bytes(b"teh world");
        // Cursor at end (9). Move left until cursor=4 (just after space,
        // before 'w'). For `teh` at 0..3, char_end=3, so left_moves=1.
        for _ in 0..5 {
            s.feed_bytes(&[0x1B, b'[', b'D']);
        }
        let teh = s
            .issues()
            .iter()
            .find(|i| i.word == "teh")
            .cloned()
            .expect("expected harper to flag `teh`");
        let fix = build_fix(&s, &teh, "the").expect("fix must build");
        assert_eq!(fix.left_moves, 1, "walk cursor back from 4 to char_end=3");
        assert_eq!(fix.right_moves, 0);
        assert_eq!(fix.backspaces, 3);
        assert_eq!(fix.replacement_text, "the");
    }

    #[test]
    fn fix_targets_most_recent_misspelling() {
        // `teh` first, then `paragprah`. The fix should target the latter.
        let state = make_state(b"teh paragprah");
        let fix = try_tab_fix(&state).expect("should fix the latest misspelling");
        assert_eq!(fix.issue_word, "paragprah");
        assert_eq!(fix.backspaces, "paragprah".len());
    }

    #[test]
    fn no_fix_when_buffer_empty() {
        let state = make_state(b"");
        assert!(try_tab_fix(&state).is_none());
    }

    #[test]
    fn no_fix_when_no_issues() {
        let state = make_state(b"hello world");
        assert!(try_tab_fix(&state).is_none());
    }

    #[test]
    fn fix_works_with_typed_text_between_misspelling_and_cursor() {
        // The previous behavior blocked Tab when anything non-whitespace
        // followed the misspelling. Now Tab fixes the LATEST misspelling
        // regardless, preserving the trailing text.
        let state = make_state(b"write teh paragpraph at teh centrre of");
        let fix = try_tab_fix(&state).expect("should be able to fix centrre");
        // centrre is at the end (latest misspelling). 38 - 28 = 10
        // backspaces to delete "centrre of", then suggestion + " of".
        assert_eq!(fix.issue_word, "centrre");
        assert!(
            fix.replacement_text.ends_with(" of"),
            "replacement should preserve the trailing ' of': {:?}",
            fix.replacement_text,
        );
    }

    #[test]
    fn feed_outcome_is_used() {
        // Smoke test that the buffer + state interaction works; not really
        // about fix logic, just ensures `FeedOutcome` is imported and the
        // construction path is exercised.
        let mut s = InputState::new();
        let out = s.feed_bytes(b"x");
        assert!(matches!(out, FeedOutcome::Updated | FeedOutcome::NoChange));
    }

    #[test]
    fn build_fix_refuses_earlier_line_lint_in_multi_line_buffer() {
        // Simulate a buffer that contains a paste-produced newline.
        // First-line "teh" must not be Tab-fixable because injecting
        // its trailing-text retype would include the `\n`, which is
        // a boundary outside paste mode and would corrupt the buffer.
        // Use the bracketed-paste machinery to put the `\n` in the
        // buffer through the same code path real pastes use.
        let mut s = InputState::new();
        s.feed_bytes(&[0x1B, b'[', b'2', b'0', b'0', b'~']);
        s.feed_bytes(b"teh fix\nfix more");
        s.feed_bytes(&[0x1B, b'[', b'2', b'0', b'1', b'~']);
        assert_eq!(s.buffer().text(), "teh fix\nfix more");
        // Hand-craft a lint at chars 0..3 (the line-1 "teh"). harper
        // would have produced this, but for the test we fabricate it
        // so the assertion is independent of harper's exact output.
        let issue = SpellIssue {
            byte_start: 0,
            byte_end: 3,
            char_start: 0,
            char_end: 3,
            word: "teh".into(),
            message: "".into(),
            suggestions: vec!["the".into()],
            category: crate::spell::IssueCategory::Spelling,
            priority: 50,
        };
        assert!(
            build_fix(&s, &issue, "the").is_none(),
            "earlier-line lint must not produce a Fix"
        );
    }

    #[test]
    fn build_fix_accepts_last_line_lint_in_multi_line_buffer() {
        // Counterpart to the previous test: line-2 "teh" IS fixable.
        let mut s = InputState::new();
        s.feed_bytes(&[0x1B, b'[', b'2', b'0', b'0', b'~']);
        s.feed_bytes(b"fix me\nfix teh more");
        s.feed_bytes(&[0x1B, b'[', b'2', b'0', b'1', b'~']);
        assert_eq!(s.buffer().text(), "fix me\nfix teh more");
        // Line-2 "teh" lives at chars 11..14 of the full buffer.
        let issue = SpellIssue {
            byte_start: 11,
            byte_end: 14,
            char_start: 11,
            char_end: 14,
            word: "teh".into(),
            message: "".into(),
            suggestions: vec!["the".into()],
            category: crate::spell::IssueCategory::Spelling,
            priority: 50,
        };
        let fix = build_fix(&s, &issue, "the").expect("last-line lint should be fixable");
        // End-of-buffer regime: trailing = " more" (after char_end).
        assert_eq!(fix.replacement_text, "the more");
        assert!(!fix.replacement_text.contains('\n'), "fix must not inject \\n");
    }
}
