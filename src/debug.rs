//! Optional debug log. Activated by `TUIPO_DEBUG_LOG=/path/to/file`.
//!
//! Lets you watch the engine work in real time before there's a UI:
//!
//! ```sh
//! TUIPO_DEBUG_LOG=/tmp/tuipo.log tuipo -- claude
//! # In another terminal:
//! tail -f /tmp/tuipo.log
//! ```
//!
//! Each line is `T+<ms_since_session_start> <kind> <payload>`. Append-mode,
//! so multiple sessions accumulate into one file. Cheap: no-op when the env
//! var is unset, no allocation on the hot path beyond formatting the line.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use crate::echo::EchoMatcher;
use crate::input::InputState;
use crate::screen::ScreenState;
use crate::spell::SpellIssue;

/// Shared file handle + serialization lock. The same file may be opened by
/// both the stdin and output threads, but each line goes through this Mutex
/// so it lands atomically rather than getting shredded with another thread's
/// output. `OnceLock` ensures we only open the file once per process even
/// when `DebugLog::from_env` is called from multiple threads.
static SHARED: OnceLock<Option<Arc<Mutex<std::fs::File>>>> = OnceLock::new();

pub struct DebugLog {
    file: Option<Arc<Mutex<std::fs::File>>>,
    start: Instant,
}

impl DebugLog {
    pub fn from_env() -> Self {
        let shared = SHARED
            .get_or_init(|| {
                std::env::var_os("TUIPO_DEBUG_LOG").and_then(|p| {
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&p)
                        .ok()
                        .map(|f| Arc::new(Mutex::new(f)))
                })
            })
            .clone();
        let log = Self {
            file: shared,
            start: Instant::now(),
        };
        // Two threads call `from_env` (stdin + output), so two BOOT lines
        // appear per session. Grep them out if it bothers you.
        log.line("BOOT", "tuipo session started");
        log
    }

    pub fn enabled(&self) -> bool {
        self.file.is_some()
    }

    pub fn log_input(&self, state: &InputState) {
        if !self.enabled() {
            return;
        }
        let text = state.buffer().text();
        let issues = state.issues();
        let payload = format!(
            "buf={text:?} issues={}",
            format_issues(issues),
        );
        self.line("INPUT", &payload);
    }

    pub fn log_boundary(&self) {
        self.line("BOUNDARY", "input reset");
    }

    pub fn log_prints(&self, chars: &str) {
        if !self.enabled() {
            return;
        }
        self.line("PRINTS", &format!("{chars:?}"));
    }

    pub fn log_paint(&self, lint_count: usize) {
        if !self.enabled() {
            return;
        }
        self.line("PAINT", &format!("painted {lint_count} issue(s)"));
    }

    pub fn log_resize(&self, cols: u16, rows: u16) {
        if !self.enabled() {
            return;
        }
        self.line("RESIZE", &format!("winch -> ({cols}x{rows})"));
    }

    pub fn log_fix(&self, original: &str, replacement: &str) {
        if !self.enabled() {
            return;
        }
        self.line("FIX", &format!("{original:?} -> {replacement:?}"));
    }

    pub fn log_matcher(&self, matcher: &EchoMatcher) {
        if !self.enabled() {
            return;
        }
        let payload = format!(
            "positions={} pending={} lints={}",
            matcher.known_positions(),
            matcher.pending_len(),
            matcher.lints().len(),
        );
        self.line("MATCHER", &payload);
    }

    pub fn log_screen(&self, state: &ScreenState) {
        if !self.enabled() {
            return;
        }
        let payload = format!(
            "cursor=({},{}) cols={} rows={} alt={} wrap={}",
            state.cursor.row,
            state.cursor.col,
            state.cols,
            state.rows,
            state.alt_screen,
            state.pending_wrap,
        );
        self.line("SCREEN", &payload);
    }

    fn line(&self, kind: &str, payload: &str) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        let elapsed = self.start.elapsed().as_millis();
        // Format the whole line first so the single write_all is atomic
        // enough; mutex serializes the line across threads.
        let line = format!("T+{elapsed:08} {kind:<8} {payload}\n");
        if let Ok(mut f) = file.lock() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }
}

fn format_issues(issues: &[SpellIssue]) -> String {
    if issues.is_empty() {
        return "[]".into();
    }
    let mut s = String::from("[");
    for (i, issue) in issues.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        let top_suggestion = issue.suggestions.first().map(String::as_str).unwrap_or("-");
        s.push_str(&format!(
            "{}@{}..{}→{:?}",
            issue.word, issue.byte_start, issue.byte_end, top_suggestion
        ));
    }
    s.push(']');
    s
}
