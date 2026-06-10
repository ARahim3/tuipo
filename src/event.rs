//! Cross-thread events.
//!
//! Stdin thread emits [`InputEvent`]s describing what the user did; the
//! output thread consumes them to update its echo-tracking state.
//!
//! The stdin thread can't share `InputState` directly because harper-core's
//! `LintGroup` is `!Send` (see `project_tuipo_constraints.md`). Channels
//! sidestep that — only the lint *snapshot* (which is `Send`) crosses
//! threads, not the linter itself.

use crate::screen::CursorPos;
use crate::spell::SpellIssue;

#[derive(Debug, Clone)]
pub enum InputEvent {
    /// User typed a single printable ASCII character. `char_offset` is the
    /// 0-based char index in the input buffer where this character now sits
    /// (i.e. the offset of the just-inserted char, *not* one past it).
    UserChar { ch: char, char_offset: usize },
    /// Input boundary — Enter / Esc / Ctrl-C / Ctrl-D. Output thread should
    /// drop its echo map and pending queue.
    Boundary,
    /// Updated lint list + the current buffer's character count + the
    /// buffer text itself + the cursor's char offset within that text.
    /// The text lets the paint side search recent `PrintEvent`s for the
    /// latest contiguous on-screen rendering of the input — far more
    /// robust than cursor-relative anchoring against hosts (Claude Code,
    /// etc.) that move the cursor off the input row after rendering.
    /// `buffer_chars` is kept as a cheap sanity bound. `buffer_cursor` is
    /// in CHAR offsets (not bytes) to match lint spans, and is used by
    /// the picker's hover detection: cursor inside a lint span + idle
    /// → tooltip.
    Lints {
        issues: Vec<SpellIssue>,
        buffer_chars: usize,
        buffer_text: String,
        buffer_cursor: usize,
    },
    /// Picker open / closed. `None` means closed; `Some(snapshot)` means the
    /// picker is open and the render loop should display this state.
    /// Currently unused — picker UI removed in favor of "Tab = apply top
    /// suggestion." Kept so adding multi-suggestion UI later is a small
    /// diff rather than re-plumbing the event channel.
    #[allow(dead_code)]
    PickerState(Option<PickerSnapshot>),
}

/// Snapshot of the suggestion picker. Sent from the stdin thread (which owns
/// the picker state machine) to the render thread (which displays it in the
/// status row).
#[derive(Debug, Clone)]
pub struct PickerSnapshot {
    /// The misspelling we're offering to fix.
    pub target: String,
    /// Top suggestions, in priority order (best first).
    pub suggestions: Vec<String>,
    /// Currently-highlighted suggestion index.
    pub selected: usize,
}

/// Emitted by the screen observer for each printable character the child
/// drew. `at` is the cursor position *before* the print happened — i.e., the
/// cell where the character is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrintEvent {
    pub ch: char,
    pub at: CursorPos,
}

/// Emitted by the screen observer when the child clears a range of cells
/// (CSI J or CSI K). The matcher uses this to drop stale grid entries
/// instead of letting them masquerade as on-screen content.
///
/// Range is `[col_start, col_end)` on `row`. For multi-row erases (e.g.
/// `\x1b[2J` clearing the whole screen) the observer emits one
/// `EraseEvent` per affected row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EraseEvent {
    pub row: u16,
    pub col_start: u16,
    pub col_end: u16,
}

/// A screen-side event the matcher should consume. We deliberately keep
/// Print and Erase as a discriminated union (rather than two separate
/// `Vec`s) so the observer can interleave them in *byte order* — that
/// matters because a `print → erase same cell → print` sequence must
/// leave the cell holding the second print, and the matcher needs to
/// see those operations in the order they happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenEvent {
    Print(PrintEvent),
    Erase(EraseEvent),
}

/// Multiplexed event the render loop pulls from a single channel. Four
/// producers (PTY reader thread, stdin thread, tick timer, winch thread)
/// push these onto one channel so the render thread can react in order
/// without needing `select!`-style multi-channel waits.
#[derive(Debug)]
pub enum RenderEvent {
    /// Raw bytes the child wrote to its tty.
    PtyBytes(Box<[u8]>),
    /// User-side activity from the stdin thread.
    Input(InputEvent),
    /// Periodic wake-up; render loop checks paint conditions on tick.
    Tick,
    /// Terminal was resized. The render loop must update the
    /// `ScreenObserver`'s dimensions BEFORE parsing any subsequent
    /// `PtyBytes` from the child's redraw — otherwise print events get
    /// clamped to the old dimensions and the grid model goes stale. The
    /// winch thread is responsible for sending this *before* it tells
    /// the PTY master about the new size, so the ordering is:
    ///
    /// 1. Winch thread → channel: `Resize { cols, rows }`
    /// 2. Winch thread → PTY master: resize → child receives SIGWINCH
    /// 3. Child → PTY reader → channel: `PtyBytes` with the redraw
    Resize { cols: u16, rows: u16 },
    /// Synchronous paint request: bypass the 50ms tick and call the
    /// painter immediately. Used by Tab-fix so the stale underline goes
    /// away in lockstep with the buffer change rather than persisting
    /// for up to one tick after the user has visibly applied a fix.
    /// Emitted by the stdin thread *after* its `Lints` event, so by the
    /// time the render loop processes it the matcher already knows the
    /// fixed-word lint is gone.
    ForcePaint,
    /// PTY closed (child exited or fd dropped).
    PtyEof,
}

