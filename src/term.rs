//! Local terminal mode management. Keeps raw mode confined to a RAII guard so
//! every exit path — `Drop`, `?` bubble, panic — restores the user's terminal.

use anyhow::{Context, Result};
use crossterm::tty::IsTty;

/// Holds the local terminal in raw mode while alive. No-op when stdin/stdout
/// aren't a TTY (e.g. integration tests, shell pipelines) so the wrapper still
/// works headless.
pub struct RawModeGuard {
    raw_enabled: bool,
}

impl RawModeGuard {
    pub fn new() -> Result<Self> {
        let on_tty = std::io::stdin().is_tty() && std::io::stdout().is_tty();
        if on_tty {
            crossterm::terminal::enable_raw_mode()
                .context("failed to enable terminal raw mode")?;
        }
        Ok(Self { raw_enabled: on_tty })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.raw_enabled {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

/// Install a panic hook that disables raw mode before the panic message is
/// printed. Otherwise the message renders without proper line breaks because
/// the terminal is still in raw mode at the point Rust prints it.
///
/// Panics on the lint worker thread are the exception: the worker contains
/// them (a harper panic costs one lint result) and the session carries on,
/// so the hook must neither drop raw mode nor print over the wrapped app.
pub fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some(crate::lint_worker::WORKER_THREAD_NAME) {
            return;
        }
        let _ = crossterm::terminal::disable_raw_mode();
        original(info);
    }));
}
