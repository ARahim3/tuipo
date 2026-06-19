//! PTY orchestration. Spawns the wrapped command under a pseudo-terminal,
//! pumps bytes both directions, forwards SIGWINCH, runs the render loop.
//!
//! Threading layout:
//! - **winch** thread: forwards SIGWINCH to PTY.
//! - **stdin pump** thread: reads user input, updates the harper-side
//!   `InputState`, emits `RenderEvent::Input` *before* writing each byte to
//!   the PTY so input events reach the channel ahead of any line-discipline
//!   echo.
//! - **pty reader** thread: reads from PTY master, emits `RenderEvent::PtyBytes`.
//! - **tick** thread: emits `RenderEvent::Tick` every `TICK_INTERVAL`.
//! - **main / render** thread: consumes `RenderEvent`s from one channel,
//!   writes to user's stdout, runs the screen observer + echo matcher, and
//!   paints annotations when paused.

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use signal_hook::consts::SIGWINCH;
use signal_hook::iterator::Signals;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::buffer::FeedOutcome;
use crate::debug::DebugLog;
use crate::echo::EchoMatcher;
use crate::event::{InputEvent, RenderEvent, ScreenEvent};
use crate::fix::{Fix, try_tab_fix};
use crate::input::InputState;
use crate::paint::{self, PaintGate};
use crate::screen::ScreenObserver;
use crate::status;
use crate::term::RawModeGuard;

/// How often the render loop wakes up to check paint conditions. 50ms is
/// well under the 150ms pause threshold, so the first paint after the user
/// stops typing lands within ~50ms of the pause becoming "long enough."
const TICK_INTERVAL: Duration = Duration::from_millis(50);

/// Opt-in switch for Tab→quick-fix behavior. Default is off so that wrapping
/// shells / TUIs see Tab with its normal meaning. Enabled by
/// `tab_fix = true` in `~/.config/tuipo/config.toml`, or by setting
/// `TUIPO_TAB_FIX=1` in the env. Enabling `picker = true` also
/// implicitly turns this on — the picker only opens via Tab.
fn tab_fix_enabled() -> bool {
    crate::config::get().tab_fix_enabled()
}

/// Convert a byte-offset cursor to a char-offset cursor within `text`.
/// `Buffer::cursor()` is in bytes (because it indexes into the underlying
/// `String`), but lint spans are in chars — they have to be comparable
/// for the picker's hover detection.
fn char_cursor_in(text: &str, byte_cursor: usize) -> usize {
    let clamped = byte_cursor.min(text.len());
    text[..clamped].chars().count()
}

pub fn run(command: Vec<String>) -> Result<i32> {
    let raw_guard = RawModeGuard::new()?;

    // Reset any lingering SGR state from a previous session so leftover
    // underline/color attributes don't carry into the new one. Cheap and
    // non-destructive — doesn't touch cell content, just the pen state
    // for the next character drawn.
    {
        use std::io::Write as _;
        let stdout = std::io::stdout();
        let mut h = stdout.lock();
        let _ = h.write_all(b"\x1b[0m");
        let _ = h.flush();
    }

    let pty_system = native_pty_system();
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open PTY")?;

    let mut cmd = CommandBuilder::new(&command[0]);
    for arg in command.iter().skip(1) {
        cmd.arg(arg);
    }
    cmd.env("TUIPO_ACTIVE", "1");
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("failed to spawn `{}`", command[0]))?;
    drop(pair.slave);

    let master = pair.master;
    let reader = master
        .try_clone_reader()
        .context("failed to clone PTY reader")?;
    let writer = master
        .take_writer()
        .context("failed to take PTY writer")?;
    let master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(master));

    // One channel, three producers (stdin pump, PTY reader, tick), one
    // consumer (render loop).
    let (event_tx, event_rx) = mpsc::channel::<RenderEvent>();

    spawn_winch_thread(Arc::clone(&master), event_tx.clone());
    spawn_stdin_pump(writer, event_tx.clone());
    spawn_pty_reader(reader, event_tx.clone());
    spawn_tick(event_tx.clone());
    drop(event_tx); // render loop holds the last receiver only

    render_loop(ScreenObserver::new(cols, rows), event_rx);

    let status = child.wait().context("failed to wait for child")?;
    let exit_code = status.exit_code() as i32;

    drop(raw_guard);
    drop(master);
    Ok(exit_code)
}

fn spawn_winch_thread(
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    event_tx: mpsc::Sender<RenderEvent>,
) {
    thread::spawn(move || {
        let debug = DebugLog::from_env();
        let mut signals = match Signals::new([SIGWINCH]) {
            Ok(s) => s,
            Err(_) => return,
        };
        for _ in signals.forever() {
            let Ok((cols, rows)) = crossterm::terminal::size() else {
                continue;
            };
            if debug.enabled() {
                debug.log_resize(cols, rows);
            }
            // Order matters: send Resize to the render channel BEFORE
            // resizing the PTY master. The render loop processes events
            // FIFO, so updating the observer's dimensions lands ahead
            // of any redraw bytes the child emits once it receives
            // SIGWINCH from the resize below.
            if event_tx.send(RenderEvent::Resize { cols, rows }).is_err() {
                break;
            }
            if let Ok(m) = master.lock() {
                let _ = m.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
        }
    });
}

fn spawn_stdin_pump(writer: Box<dyn Write + Send>, event_tx: mpsc::Sender<RenderEvent>) {
    thread::spawn(move || pump_stdin_to_pty(writer, event_tx));
}

/// Minimal escape-sequence parser used by the stdin pump while the
/// picker is engaged. We only need to recognise arrow keys (CSI A/B/C/D)
/// and bare Esc — everything else falls through to normal byte
/// processing. Active only when [`PickerCtx::engaged`] is `Some`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EscState {
    Ground,
    SawEsc,
    SawCsi,
}

struct PickerCtx {
    engaged: Option<crate::picker::Engaged>,
    esc: EscState,
    /// Buffer char count after the most recent buffer-changing keystroke.
    /// Lets `picker_engage` tell whether the last edit grew or shrank the
    /// buffer — the same signal the render-side hover gate keys on to
    /// decide whether the picker may surface at end-of-buffer.
    last_buffer_chars: Option<usize>,
    /// True when the most recent buffer change reduced the char count
    /// (backspace / Ctrl-W / Delete). Reset on Boundary so a fresh
    /// prompt's first keystroke can't masquerade as a shrink. Mirrors
    /// `OverlayGate::buffer_shrank` on the render side.
    buffer_shrank: bool,
}

impl PickerCtx {
    fn new() -> Self {
        Self {
            engaged: None,
            esc: EscState::Ground,
            last_buffer_chars: None,
            buffer_shrank: false,
        }
    }

    /// Update the grow/shrink tracker after a buffer-changing keystroke.
    fn note_buffer(&mut self, buffer_chars: usize) {
        self.buffer_shrank = matches!(self.last_buffer_chars, Some(prev) if buffer_chars < prev);
        self.last_buffer_chars = Some(buffer_chars);
    }

    /// Drop grow/shrink tracking on Boundary (Enter / Esc / Ctrl-C /
    /// Ctrl-D) so the next prompt starts clean — otherwise its first
    /// keystroke (`buffer_chars = 1`) would compare against the previous
    /// prompt's length and falsely register as a shrink.
    fn reset_buffer_tracking(&mut self) {
        self.last_buffer_chars = None;
        self.buffer_shrank = false;
    }
}

fn pump_stdin_to_pty(mut writer: Box<dyn Write + Send>, event_tx: mpsc::Sender<RenderEvent>) {
    let mut state = InputState::new();
    let debug = DebugLog::from_env();
    let mut buf = [0u8; 4096];
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();
    let mut picker = PickerCtx::new();
    loop {
        match handle.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                // Paste burst: forward the whole chunk to the child at
                // once and run harper exactly once afterward, rather than
                // once per byte. The per-byte path below spell-checks the
                // entire buffer on every keystroke; for a large paste
                // that's O(n²) harper passes AND the byte isn't forwarded
                // until each pass finishes — the multi-second hang in
                // GH #1. A paste isn't interactive input, so none of the
                // picker / Tab-fix machinery in `handle_byte` applies.
                if chunk_looks_like_paste(chunk) {
                    if !handle_paste_chunk(
                        chunk,
                        &mut writer,
                        &mut state,
                        &event_tx,
                        &debug,
                        &mut picker,
                    ) {
                        break;
                    }
                    continue;
                }
                let mut send_failed = false;
                for &byte in chunk {
                    handle_byte(
                        byte,
                        &mut writer,
                        &mut state,
                        &event_tx,
                        &debug,
                        &mut send_failed,
                        &mut picker,
                    );
                    if send_failed {
                        break;
                    }
                }
                if send_failed {
                    break;
                }
                // After processing the read batch, finalize the escape
                // state. A bare Esc lingers as `SawEsc` until the next
                // byte arrives; treating it here as "user pressed Esc
                // alone, no CSI followed" lets us dismiss the picker.
                if picker.engaged.is_some()
                    && picker.esc != EscState::Ground
                {
                    picker_dismiss(&mut picker, &event_tx);
                }
                if writer.flush().is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// Handle a read chunk classified as a paste burst (see
/// [`chunk_looks_like_paste`]). Reconstructs the input buffer without
/// spell-checking each intermediate prefix, forwards the whole chunk to
/// the child *before* the spell pass so the wrapped app shows
/// "[Pasted N lines]" without delay, then runs harper exactly once and
/// emits a single `Lints` snapshot. This is the fix for the multi-second
/// paste hang (GH #1): per-byte linting is O(n²) harper passes and stalls
/// byte forwarding until each finishes.
///
/// Returns `false` if writing to the PTY failed (the caller should stop
/// the pump).
fn handle_paste_chunk(
    chunk: &[u8],
    writer: &mut Box<dyn Write + Send>,
    state: &mut InputState,
    event_tx: &mpsc::Sender<RenderEvent>,
    debug: &DebugLog,
    picker: &mut PickerCtx,
) -> bool {
    // `chunk_paste` keeps interior CR/LF as buffer content so a multi-line
    // paste doesn't Boundary mid-stream on hosts that don't enable
    // bracketed paste (Claude Code). Marker-driven `in_paste` (for hosts
    // that do) still toggles independently as the buffer sees `\x1b[200~`
    // / `\x1b[201~` inside this same chunk.
    state.set_chunk_paste(true);
    state.feed_bytes_deferred(chunk);
    state.set_chunk_paste(false);
    // Get the bytes to the child first — the spell pass comes after.
    if writer.write_all(chunk).is_err() {
        return false;
    }
    if writer.flush().is_err() {
        return false;
    }
    // One harper pass over the final buffer, then a single Lints event.
    // The painter anchors off `buffer_text` + the screen grid, so a lone
    // snapshot is all it needs (the per-byte `UserChar` events the normal
    // path emits only feed the non-load-bearing legacy pairing + the
    // paint throttle, neither of which matters for a paste).
    state.refresh();
    let buffer_text = state.buffer().text().to_string();
    let buffer_chars = buffer_text.chars().count();
    let buffer_cursor = char_cursor_in(&buffer_text, state.buffer().cursor());
    // Mirror the grow/shrink tracker the per-byte path maintains, so a Tab
    // right after a paste evaluates the picker gate against the right
    // buffer length.
    picker.note_buffer(buffer_chars);
    let _ = event_tx.send(RenderEvent::Input(InputEvent::Lints {
        issues: state.issues().to_vec(),
        buffer_chars,
        buffer_text,
        buffer_cursor,
    }));
    if debug.enabled() {
        debug.log_input(state);
    }
    true
}

/// Minimum size for a CR/LF-ended chunk to be classified as a paste
/// line. A typed Enter alone is 1 byte; a coalesced burst of fast
/// typing then Enter is rarely above 10. Pasted prose lines run dozens
/// of bytes (the user's repro pastes are ~70+ chars per line). 16 is
/// roomy enough to never catch typing yet catches even the shortest
/// realistic paste line.
const MIN_PASTE_CHUNK_SIZE: usize = 16;

/// A read chunk at least this large is treated as a paste even with no
/// CR/LF anywhere in it — i.e. a single long line pasted in one go (a
/// paragraph with no hard breaks). Comfortably above any escape sequence
/// (an SGR mouse report is ~12 bytes; the longest CSI we'd ever see is
/// well under 32) and above any realistic single-`read()` typing burst in
/// raw mode, where each keypress wakes the reader. A false positive is
/// harmless: it only batches the spell pass for that chunk, alters no
/// bytes, and — having no newline — can't mis-handle a boundary.
const MIN_PASTE_NOLINE_SIZE: usize = 64;

/// Decide whether a single stdin read chunk represents a paste.
///
/// In raw mode, an interactive keypress produces at most one short
/// burst per `read()` and `Enter` always sits at the *end* of its
/// chunk (the read blocks until input is available, and `Enter`
/// flushes the terminal's line). The signatures below cannot be
/// produced by typing, so when we see them we know the chunk came
/// from a paste burst — even when the host (Claude Code as of v2.x)
/// didn't enable bracketed-paste mode and the wrapping terminal
/// therefore didn't emit `\x1b[200~` markers.
///
/// Two signatures:
/// 1. **Content after a newline.** Strip any trailing CR/LF run,
///    then look at what remains. If there's still a CR or LF inside,
///    the chunk holds more than one line — impossible from typing
///    because Enter would have ended the read. Catches pastes
///    delivered as one big chunk (Zed / VS Code / native Mac Terminal).
/// 2. **CR/LF-ended chunk larger than any plausible single keystroke
///    burst.** Catches multiplexers (cmux, tmux) and terminals that
///    deliver paste content line-by-line — one read per line, each
///    ending with `\n` and *no* interior `\n`. Without this signal,
///    every pasted line would individually look like "user typed
///    some content then pressed Enter" to signal #1, and each `\n`
///    would boundary the buffer.
/// 3. **Any chunk ≥ `MIN_PASTE_NOLINE_SIZE` (64) bytes**, even with no
///    CR/LF at all. Catches a long single-line paste (a paragraph with
///    no hard breaks). Typing and escape sequences never reach this
///    size in one read; a false positive only batches the spell pass.
///
/// False-positive analysis:
/// - `"hello\r"` (typed "hello" + Enter, coalesced): trim leaves
///   `"hello"`, no inner newline. Length 6 < 16. NOT a paste. ✓
/// - `"\r\n"` (Enter on a CRLF terminal): trims to empty. Length 2
///   < 16. NOT a paste. ✓
/// - `"this is a longer line typed at 200wpm with Enter\n"` (~50
///   bytes): would trigger signal 2 → false positive. Users typing
///   that fast in one chunk are rare in practice; if it does happen,
///   the only consequence is that our buffer keeps the trailing `\n`
///   as content (Claude still receives the byte and submits). The
///   stale buffer's painting is filtered by the grid sanity check
///   in `paint::compute_new_spans` (cell mismatch → skip), so no
///   visible misalignment.
fn chunk_looks_like_paste(chunk: &[u8]) -> bool {
    let trailing = chunk
        .iter()
        .rev()
        .take_while(|&&b| b == b'\n' || b == b'\r')
        .count();
    let body = &chunk[..chunk.len() - trailing];
    // Signal 1: content after a newline in the same chunk.
    if body.iter().any(|&b| b == b'\n' || b == b'\r') {
        return true;
    }
    // Signal 2: a CR/LF-ended chunk that's too big to be typing.
    if trailing > 0 && chunk.len() >= MIN_PASTE_CHUNK_SIZE {
        return true;
    }
    // Signal 3: a chunk large enough that no keystroke / escape sequence
    // could have produced it, even with no newline at all.
    if chunk.len() >= MIN_PASTE_NOLINE_SIZE {
        return true;
    }
    false
}

/// Tear down the engaged picker without applying anything. Emits the
/// PickerState(None) event so the render thread restores the cells we
/// drew over.
fn picker_dismiss(picker: &mut PickerCtx, event_tx: &mpsc::Sender<RenderEvent>) {
    if picker.engaged.take().is_some() {
        let _ = event_tx.send(RenderEvent::Input(InputEvent::PickerState(None)));
        let _ = event_tx.send(RenderEvent::ForcePaint);
    }
    picker.esc = EscState::Ground;
}

/// Try to engage the picker on the cursor's current lint. Returns true
/// when engagement succeeded (the caller should NOT forward Tab to the
/// PTY in that case).
fn picker_engage(
    state: &InputState,
    picker: &mut PickerCtx,
    event_tx: &mpsc::Sender<RenderEvent>,
) -> bool {
    let buffer_text = state.buffer().text();
    let buffer_cursor = char_cursor_in(buffer_text, state.buffer().cursor());
    let buffer_chars = buffer_text.chars().count();
    // Don't hijack Tab while the user is typing forward at end-of-buffer —
    // that's exactly when the wrapped child (a shell, etc.) wants Tab for
    // its own completion. Engage only once the user shows edit intent.
    // Same gate as the render-side hover tooltip, so Tab engages iff the
    // tooltip is (or would be) showing. See `picker::typing_forward_at_end`
    // and decision #11.
    if crate::picker::typing_forward_at_end(buffer_cursor, buffer_chars, picker.buffer_shrank) {
        return false;
    }
    let grammar_enabled = crate::config::get().grammar_enabled();
    let Some(target) = crate::picker::pick_target(state.issues(), buffer_cursor, grammar_enabled)
    else {
        return false;
    };
    let Some(engaged) = crate::picker::engage_for(target) else {
        return false;
    };
    let snap = engaged.snapshot();
    picker.engaged = Some(engaged);
    picker.esc = EscState::Ground;
    let _ = event_tx.send(RenderEvent::Input(InputEvent::PickerState(Some(snap))));
    let _ = event_tx.send(RenderEvent::ForcePaint);
    true
}

/// Push the current Engaged snapshot to the render thread (used after
/// arrow nav so the highlight moves).
fn picker_resnap(picker: &PickerCtx, event_tx: &mpsc::Sender<RenderEvent>) {
    if let Some(e) = picker.engaged.as_ref() {
        let _ = event_tx.send(RenderEvent::Input(InputEvent::PickerState(Some(
            e.snapshot(),
        ))));
        let _ = event_tx.send(RenderEvent::ForcePaint);
    }
}

/// Apply the engaged picker's selected suggestion via the standard fix
/// path. Returns true on success; on success the picker is cleared.
fn picker_apply(
    writer: &mut Box<dyn Write + Send>,
    state: &mut InputState,
    picker: &mut PickerCtx,
    event_tx: &mpsc::Sender<RenderEvent>,
    debug: &DebugLog,
) -> bool {
    let Some(eng) = picker.engaged.take() else {
        return false;
    };
    let suggestion = eng.selected_suggestion().to_string();
    // Find the live issue that matches our engaged span. The stored
    // engaged.target_char_start/end is in char offsets at engagement
    // time; lints may have shifted if the user backspaced inside the
    // word during a passive hover, but in engaged mode the user only
    // presses arrows/Enter/Esc, so the span shouldn't move.
    let issue_clone = state
        .issues()
        .iter()
        .find(|i| {
            i.char_start == eng.target_char_start
                && i.char_end == eng.target_char_end
                && i.word == eng.target_word
        })
        .cloned();
    let Some(issue) = issue_clone else {
        // The matching lint disappeared (e.g. user moved cursor away).
        // Drop the picker without touching the buffer.
        let _ = event_tx.send(RenderEvent::Input(InputEvent::PickerState(None)));
        let _ = event_tx.send(RenderEvent::ForcePaint);
        picker.esc = EscState::Ground;
        return false;
    };
    let Some(fix) = crate::fix::build_fix(state, &issue, &suggestion) else {
        let _ = event_tx.send(RenderEvent::Input(InputEvent::PickerState(None)));
        let _ = event_tx.send(RenderEvent::ForcePaint);
        picker.esc = EscState::Ground;
        return false;
    };
    let _ = apply_fix(writer, state, event_tx, debug, &fix);
    // Close the picker after applying (apply_fix already sent Lints +
    // ForcePaint, so the underline + overlay both clear).
    let _ = event_tx.send(RenderEvent::Input(InputEvent::PickerState(None)));
    let _ = event_tx.send(RenderEvent::ForcePaint);
    picker.esc = EscState::Ground;
    true
}

fn handle_byte(
    byte: u8,
    writer: &mut Box<dyn Write + Send>,
    state: &mut InputState,
    event_tx: &mpsc::Sender<RenderEvent>,
    debug: &DebugLog,
    send_failed: &mut bool,
    picker: &mut PickerCtx,
) {
    // Engaged picker captures arrows / Enter / Esc / Tab and nothing
    // else. Everything else falls through to the normal byte path so
    // the user can keep typing during an active overlay (typing
    // dismisses the picker first).
    //
    // Navigation: Left/Right is the primary direction because the picker
    // overlay is laid out horizontally. Up/Down also nav as a muscle-
    // memory affordance for users coming from vertical menus. Tab cycles
    // forward like Right (the same "next" shorthand most completion
    // menus use); Shift-Tab cycles backward via `\x1b[Z`.
    // Apply: Enter only. Tab used to apply too, but cycling is more
    // discoverable and Enter is unambiguous for "I picked this one."
    if picker.engaged.is_some() {
        match (picker.esc, byte) {
            (EscState::Ground, 0x1B) => {
                picker.esc = EscState::SawEsc;
                return;
            }
            (EscState::SawEsc, b'[') => {
                picker.esc = EscState::SawCsi;
                return;
            }
            (EscState::SawEsc, _) => {
                // Bare Esc was the previous byte → dismiss. Then
                // reprocess this byte as a fresh keystroke.
                picker_dismiss(picker, event_tx);
                // fall through
            }
            (EscState::SawCsi, b'A') | (EscState::SawCsi, b'D') | (EscState::SawCsi, b'Z') => {
                // Up / Left / Shift-Tab → previous suggestion.
                if let Some(e) = picker.engaged.as_mut() {
                    e.nav(-1);
                }
                picker_resnap(picker, event_tx);
                picker.esc = EscState::Ground;
                return;
            }
            (EscState::SawCsi, b'B') | (EscState::SawCsi, b'C') => {
                // Down / Right → next suggestion.
                if let Some(e) = picker.engaged.as_mut() {
                    e.nav(1);
                }
                picker_resnap(picker, event_tx);
                picker.esc = EscState::Ground;
                return;
            }
            (EscState::SawCsi, _) => {
                // Some other CSI sequence (function key, mouse, etc.).
                // Picker doesn't care — dismiss and let the child see
                // the sequence in full. We can't un-consume the ESC and
                // `[` we already swallowed, so forward them now alongside
                // this final byte.
                picker_dismiss(picker, event_tx);
                let _ = writer.write_all(&[0x1B, b'[', byte]);
                return;
            }
            (EscState::Ground, 0x0D | 0x0A) => {
                // Enter applies the highlighted suggestion.
                picker_apply(writer, state, picker, event_tx, debug);
                return;
            }
            (EscState::Ground, b'\t') => {
                // Tab cycles forward through suggestions. After
                // engaging, repeated Tab presses walk the highlight; the
                // user commits with Enter.
                if let Some(e) = picker.engaged.as_mut() {
                    e.nav(1);
                }
                picker_resnap(picker, event_tx);
                return;
            }
            (EscState::Ground, _) => {
                // Anything else → dismiss; let the byte be processed
                // normally below.
                picker_dismiss(picker, event_tx);
            }
        }
    }

    // Tab: spell-fix path. If the picker is enabled and we can identify
    // a target lint, engage the picker instead of auto-applying. Tab-fix
    // without picker still auto-applies the top suggestion.
    if byte == b'\t' && tab_fix_enabled() {
        if crate::config::get().picker_enabled() {
            if picker_engage(state, picker, event_tx) {
                return;
            }
        } else if let Some(fix) = try_tab_fix(state) {
            if apply_fix(writer, state, event_tx, debug, &fix).is_err() {
                *send_failed = true;
            }
            return;
        }
    }

    let cursor_before = state.buffer().cursor();
    let outcome = state.feed_bytes(&[byte]);
    match outcome {
        FeedOutcome::Updated => {
            let cursor_after = state.buffer().cursor();
            if (byte.is_ascii_graphic() || byte == b' ') && cursor_after > cursor_before {
                let char_offset = state.buffer().text()[..cursor_before].chars().count();
                let _ = event_tx.send(RenderEvent::Input(InputEvent::UserChar {
                    ch: byte as char,
                    char_offset,
                }));
            }
            let buffer_text = state.buffer().text().to_string();
            let buffer_chars = buffer_text.chars().count();
            let buffer_cursor = char_cursor_in(&buffer_text, state.buffer().cursor());
            // Track grow/shrink so the next Tab knows whether the user is
            // typing forward (Tab → child completion) or editing back into
            // a word (Tab → engage the picker).
            picker.note_buffer(buffer_chars);
            let _ = event_tx.send(RenderEvent::Input(InputEvent::Lints {
                issues: state.issues().to_vec(),
                buffer_chars,
                buffer_text,
                buffer_cursor,
            }));
            if debug.enabled() {
                debug.log_input(state);
            }
        }
        FeedOutcome::Boundary => {
            picker.reset_buffer_tracking();
            let _ = event_tx.send(RenderEvent::Input(InputEvent::Boundary));
            if debug.enabled() {
                debug.log_boundary();
            }
        }
        FeedOutcome::NoChange => {}
    }
    if writer.write_all(&[byte]).is_err() {
        *send_failed = true;
    }
}

/// Apply a tab-fix: inject (optional) arrow keys + backspaces + replacement
/// bytes into the PTY's stdin and into our local buffer in lockstep,
/// emitting `UserChar`/`Lints` events so the matcher and renderer track
/// the corrected text. Arrow keys are only used by the mid-buffer picker
/// case where the user moved their cursor into a flagged word — we walk
/// the cursor to `char_end` before backspacing so we don't delete the
/// wrong chars.
fn apply_fix(
    writer: &mut Box<dyn Write + Send>,
    state: &mut InputState,
    event_tx: &mpsc::Sender<RenderEvent>,
    debug: &DebugLog,
    fix: &Fix,
) -> std::io::Result<()> {
    if debug.enabled() {
        debug.log_fix(&fix.issue_word, &fix.replacement_text);
    }
    // Position the cursor at `char_end` of the misspelling. Each arrow
    // CSI is forwarded to the PTY (so the child moves its cursor) and
    // fed to our local buffer (so its cursor tracks the child's). Arrow
    // CSIs are 3 bytes (`\x1b[C` / `\x1b[D`); feeding them through
    // `state.feed_bytes` exercises the CSI parser already wired up for
    // user-typed arrows. Non-zero for the picker mid-buffer fix only —
    // legacy Tab-fix paths leave both counts at 0.
    for _ in 0..fix.left_moves {
        writer.write_all(b"\x1b[D")?;
        state.feed_bytes(b"\x1b[D");
    }
    for _ in 0..fix.right_moves {
        writer.write_all(b"\x1b[C")?;
        state.feed_bytes(b"\x1b[C");
    }
    // Backspaces: feed to local state and inject to PTY. We don't emit
    // UserChar for these (backspace isn't a printable char) — the buffer
    // simply shrinks.
    for _ in 0..fix.backspaces {
        writer.write_all(&[0x08])?;
        state.feed_bytes(&[0x08]);
    }
    // Replacement bytes. ASCII-only path (the typed misspellings we fix
    // are always ASCII; the suggestion text is too).
    for byte in fix.replacement_text.bytes() {
        let cursor_before = state.buffer().cursor();
        state.feed_bytes(&[byte]);
        let cursor_after = state.buffer().cursor();
        if (byte.is_ascii_graphic() || byte == b' ') && cursor_after > cursor_before {
            let char_offset = state.buffer().text()[..cursor_before].chars().count();
            let _ = event_tx.send(RenderEvent::Input(InputEvent::UserChar {
                ch: byte as char,
                char_offset,
            }));
        }
        writer.write_all(&[byte])?;
    }
    // Lint snapshot is now fresh; tell the matcher.
    let buffer_text = state.buffer().text().to_string();
    let buffer_chars = buffer_text.chars().count();
    let buffer_cursor = char_cursor_in(&buffer_text, state.buffer().cursor());
    let _ = event_tx.send(RenderEvent::Input(InputEvent::Lints {
        issues: state.issues().to_vec(),
        buffer_chars,
        buffer_text,
        buffer_cursor,
    }));
    // Ask the render loop to paint synchronously so the underline on the
    // just-fixed word vanishes in lockstep with the Tab press. Without
    // this the stale-underline clear has to wait for the next 50ms tick,
    // which feels laggy because the host's echo of the replacement bytes
    // is almost always faster than a tick.
    let _ = event_tx.send(RenderEvent::ForcePaint);
    writer.flush()?;
    Ok(())
}

fn spawn_pty_reader(reader: Box<dyn Read + Send>, event_tx: mpsc::Sender<RenderEvent>) {
    thread::spawn(move || pump_pty_reader(reader, event_tx));
}

fn pump_pty_reader(mut reader: Box<dyn Read + Send>, event_tx: mpsc::Sender<RenderEvent>) {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                let _ = event_tx.send(RenderEvent::PtyEof);
                break;
            }
            Ok(n) => {
                let bytes: Box<[u8]> = buf[..n].into();
                if event_tx.send(RenderEvent::PtyBytes(bytes)).is_err() {
                    break;
                }
            }
            Err(_) => {
                let _ = event_tx.send(RenderEvent::PtyEof);
                break;
            }
        }
    }
}

fn spawn_tick(event_tx: mpsc::Sender<RenderEvent>) {
    thread::spawn(move || {
        loop {
            thread::sleep(TICK_INTERVAL);
            if event_tx.send(RenderEvent::Tick).is_err() {
                break;
            }
        }
    });
}

fn render_loop(mut observer: ScreenObserver, event_rx: mpsc::Receiver<RenderEvent>) {
    let mut matcher = EchoMatcher::new();
    let mut gate = PaintGate::new();
    let mut overlay_gate = crate::picker::OverlayGate::new();
    let mut screen_events: Vec<ScreenEvent> = Vec::with_capacity(64);
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let debug = DebugLog::from_env();
    let mut last_screen_version = observer.state().version;
    let mut last_matcher_positions = 0usize;
    let hover_window = crate::config::get().hover_duration();

    while let Ok(ev) = event_rx.recv() {
        match ev {
            RenderEvent::PtyBytes(bytes) => {
                if handle.write_all(&bytes).is_err() {
                    break;
                }
                if handle.flush().is_err() {
                    break;
                }
                screen_events.clear();
                observer.observe_with_events(&bytes, &mut screen_events);
                if debug.enabled() && !screen_events.is_empty() {
                    let chars: String = screen_events
                        .iter()
                        .filter_map(|e| match e {
                            ScreenEvent::Print(p) => Some(p.ch),
                            ScreenEvent::Erase(_) => None,
                        })
                        .collect();
                    if !chars.is_empty() {
                        debug.log_prints(&chars);
                    }
                }
                for ev in &screen_events {
                    matcher.apply_screen_event(ev);
                }
                gate.mark_dirty();
                if debug.enabled() {
                    let v = observer.state().version;
                    let pos = matcher.known_positions();
                    if v != last_screen_version || pos != last_matcher_positions {
                        debug.log_screen(observer.state());
                        debug.log_matcher(&matcher);
                        last_screen_version = v;
                        last_matcher_positions = pos;
                    }
                }
            }
            RenderEvent::Resize { cols, rows } => {
                // Forward to the observer so subsequent print events get
                // placed at the right coordinates, and drop the grid —
                // old (row, col) positions are now meaningless and would
                // cause phantom-anchor matches. Also drop the painter's
                // span tracking; old (row, col) underlines are about to
                // be overwritten by the host's resize redraw, so we
                // shouldn't try to clear them via the now-empty grid.
                observer.resize(cols, rows);
                matcher.clear_screen();
                gate.mark_dirty();
                gate.forget_painted();
                if debug.enabled() {
                    debug.log_screen(observer.state());
                }
            }
            RenderEvent::Input(InputEvent::PickerState(snap)) => {
                // Stdin pump updated the engaged picker. Mirror into the
                // overlay gate so the renderer knows what (and whether)
                // to draw, then mark dirty so the next tick repaints.
                overlay_gate.set_engaged(snap);
                gate.mark_dirty();
            }
            RenderEvent::Input(input_ev) => {
                let is_keystroke = matches!(input_ev, InputEvent::UserChar { .. });
                let is_boundary = matches!(input_ev, InputEvent::Boundary);
                // Feed the cursor into the overlay's hover tracker
                // before we hand the event to the matcher (matcher
                // overwrites its own buffer_cursor as a side-effect).
                // `buffer_chars` is required too: the overlay uses
                // shrink-direction tracking to allow the hover gate to
                // fire after backspace-edit at end-of-buffer.
                if let InputEvent::Lints {
                    buffer_cursor,
                    buffer_chars,
                    ..
                } = &input_ev
                {
                    overlay_gate.note_buffer(*buffer_cursor, *buffer_chars, Instant::now());
                }
                matcher.apply_input(input_ev);
                if is_keystroke {
                    gate.mark_keystroke();
                } else {
                    gate.mark_dirty();
                }
                if is_boundary {
                    // Boundary clears the matcher grid; old painted-span
                    // tracking is meaningless against an empty grid and
                    // the host is about to redraw the prompt anyway. Same
                    // logic applies to the picker overlay — if we were
                    // showing one, it's about to be torn down. The
                    // shrink-tracker is reset so the next prompt's first
                    // keystroke (buffer_chars=1) doesn't compare against
                    // a stale "previous buffer was 10 chars" and
                    // falsely register as a shrink.
                    gate.forget_painted();
                    overlay_gate.set_engaged(None);
                    overlay_gate.region = None;
                    overlay_gate.reset_buffer_tracking();
                }
            }
            RenderEvent::ForcePaint => {
                // Synchronous paint, used by Tab-fix to clear the stale
                // underline immediately. The previous Lints event (sent
                // right before ForcePaint from the same thread) already
                // updated the matcher, so this paint sees the post-fix
                // lint set and emits the diff (clear pass) right away.
                let _ = paint::paint(&mut handle, &mut gate, &matcher, observer.state());
                let _ = crate::picker::paint_overlay(
                    &mut handle,
                    &mut overlay_gate,
                    &matcher,
                    observer.state(),
                );
            }
            RenderEvent::Tick => {
                // Re-evaluate the picker hover before drawing — cursor
                // may have been idle long enough that the tooltip should
                // appear (or vanish if the cursor just moved out).
                crate::picker::check_hover(
                    &mut overlay_gate,
                    &matcher,
                    Instant::now(),
                    hover_window,
                );
                // Paint every tick once the pause window elapses. Claude
                // and friends redraw constantly; if we skip paints based on
                // our own state signature, the child wipes our underlines
                // and they never reappear until the user types again.
                if gate.should_paint(Instant::now()) {
                    let painted = paint::paint(&mut handle, &mut gate, &matcher, observer.state());
                    let sig = paint::paint_signature(&matcher, observer.state());
                    gate.record_paint(sig);
                    if painted && debug.enabled() {
                        debug.log_paint(matcher.lints().len());
                    }
                }
                // Keep painting on every tick even when not "dirty" — Claude
                // may have wiped us between ticks. The gate's dirty flag is
                // for THROTTLING the first paint after a keystroke, not for
                // skipping continuous repaints.
                let _ = paint::paint(&mut handle, &mut gate, &matcher, observer.state());
                // Picker overlay paints after the underline so its box
                // sits ON TOP of any underlined cells (visually correct;
                // the underline stays as ambient feedback).
                let _ = crate::picker::paint_overlay(
                    &mut handle,
                    &mut overlay_gate,
                    &matcher,
                    observer.state(),
                );
                let bytes = status::build_status(&matcher, observer.state());
                if !bytes.is_empty() {
                    let _ = handle.write_all(&bytes);
                    let _ = handle.flush();
                }
            }
            RenderEvent::PtyEof => break,
        }
    }
}

#[cfg(test)]
mod chunk_paste_tests {
    use super::chunk_looks_like_paste;

    #[test]
    fn single_byte_typing_is_not_paste() {
        assert!(!chunk_looks_like_paste(b"a"));
        assert!(!chunk_looks_like_paste(b"\r"));
        assert!(!chunk_looks_like_paste(b"\n"));
    }

    #[test]
    fn enter_at_end_of_typing_burst_is_not_paste() {
        // User typed "hello" then Enter, coalesced by the OS into a
        // single read. The trailing CR/LF strips off; the body "hello"
        // has no internal newline. Must NOT be flagged as paste,
        // otherwise the boundary wouldn't fire and the user's input
        // would never get submitted.
        assert!(!chunk_looks_like_paste(b"hello\r"));
        assert!(!chunk_looks_like_paste(b"hello\n"));
        assert!(!chunk_looks_like_paste(b"hello\r\n"));
    }

    #[test]
    fn empty_enter_alone_is_not_paste() {
        assert!(!chunk_looks_like_paste(b"\r"));
        assert!(!chunk_looks_like_paste(b"\n"));
        assert!(!chunk_looks_like_paste(b"\r\n"));
    }

    #[test]
    fn multi_line_paste_is_paste() {
        // Two pasted lines: content after the inner newline survives
        // the trailing trim and trips the inner-newline check.
        assert!(chunk_looks_like_paste(b"line one\nline two"));
        assert!(chunk_looks_like_paste(b"abc\ndef\nghi"));
    }

    #[test]
    fn pasted_line_with_trailing_newline_is_paste() {
        // Multi-line paste that happens to end with a newline still
        // qualifies: trimming the trailing newline leaves "abc\ndef",
        // which has an inner newline.
        assert!(chunk_looks_like_paste(b"abc\ndef\n"));
        assert!(chunk_looks_like_paste(b"abc\r\ndef\r\n"));
    }

    #[test]
    fn typing_without_newline_is_not_paste() {
        // No newline at all → never a paste signal, regardless of how
        // many bytes coalesced (e.g. fast-typed escape sequence).
        assert!(!chunk_looks_like_paste(b""));
        assert!(!chunk_looks_like_paste(b"\x1b[A"));
        assert!(!chunk_looks_like_paste(b"hello world"));
    }

    #[test]
    fn line_by_line_paste_via_multiplexer_is_paste() {
        // cmux / tmux / iTerm in some configs deliver paste content
        // one line per read — each chunk ends with `\n` and has no
        // interior `\n`. Signal 1 misses these; signal 2 (CR/LF-ended
        // chunk >= MIN_PASTE_CHUNK_SIZE) catches them. The constant
        // MIN_PASTE_CHUNK_SIZE = 16 sits comfortably above any
        // plausible single-keystroke burst (rarely above 10 bytes)
        // and well below realistic paste-line lengths.
        let line1 = b"I'm setting up the new docker environment.\n";
        assert!(line1.len() >= super::MIN_PASTE_CHUNK_SIZE);
        assert!(chunk_looks_like_paste(line1));

        // CRLF variant.
        let line2 = b"For all intensive purposes, we need to nip\r\n";
        assert!(chunk_looks_like_paste(line2));
    }

    #[test]
    fn short_typed_line_then_enter_is_not_paste() {
        // Just below the threshold: 6-15 byte chunks ending in CR/LF
        // are still classified as typing. A typist hitting Enter after
        // a few words wouldn't reach 16 bytes per read.
        assert!(!chunk_looks_like_paste(b"hi there\r"));    // 9 bytes
        assert!(!chunk_looks_like_paste(b"go on\r\n"));     // 7 bytes
        assert!(!chunk_looks_like_paste(b"abcdefghijklmn\r")); // 15 bytes
    }

    #[test]
    fn long_single_line_paste_with_no_newline_is_paste() {
        // Signal 3: a long single-line paste (a paragraph with no hard
        // breaks) arrives in one read with no CR/LF at all. Signals 1
        // and 2 both miss it; the size threshold catches it so the spell
        // pass is batched instead of run per byte (GH #1).
        let line = b"the quick brown fox jumps over the lazy dog and then keeps running";
        assert!(line.len() >= super::MIN_PASTE_NOLINE_SIZE);
        assert!(chunk_looks_like_paste(line));
    }

    #[test]
    fn moderate_no_newline_chunk_is_not_paste() {
        // Below the no-newline threshold, a chunk without any CR/LF is
        // still treated as typing — escape sequences and short bursts
        // must not be misclassified.
        assert!(!chunk_looks_like_paste(b"hello world")); // 11 bytes
        assert!(!chunk_looks_like_paste(b"\x1b[A"));       // arrow key
        let mouse = b"\x1b[<35;120;30M"; // SGR mouse report, ~13 bytes
        assert!(!chunk_looks_like_paste(mouse));
    }
}
