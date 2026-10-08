//! Lint worker: spell-checking off the stdin thread.
//!
//! The stdin pump used to run harper synchronously on every keystroke,
//! *before* forwarding the byte to the child, and held harper's ~100 MB
//! for the life of the process. Now it only updates its buffer and hands
//! the text to this worker without waiting. The worker owns the spell
//! engine — normally a `tuipo __engine` child process (`engine.rs`) that
//! starts on the first keystroke and exits after [`DEFAULT_IDLE`] without
//! typing, so an idle terminal holds no harper memory at all — and emits
//! `InputEvent::Lints` to the render loop as results land.
//!
//! ## Invariants
//!
//! - **Each `Lints` event is self-consistent**: its issues were computed
//!   from exactly its `buffer_text`. The painter anchors off that text on
//!   the screen grid, so a snapshot that trails the live buffer by a
//!   keystroke paints in the right place or not at all — never offset.
//! - **Nothing from before a `Boundary` lands after it.** The stdin pump
//!   bumps the epoch and sends `Boundary` under the same lock the worker
//!   holds while it checks the epoch and sends `Lints`
//!   ([`LintHandle::boundary`], `Worker::publish`), so a late result for
//!   a submitted line can't repopulate the matcher.
//! - **Superseded results are dropped.** If newer text is already waiting
//!   when a result arrives, the stale one is discarded; the newer one is
//!   at most one engine round-trip behind.
//! - **Tab-fix and the picker act only on lints for the live text**:
//!   [`LintHandle::issues_for`] waits (bounded) until the snapshot for
//!   exactly that text has been published.
//!
//! ## Engine lifecycle
//!
//! `Stopped` → spawn on demand → `Starting` → hello → `Ready` → idle for
//! [`DEFAULT_IDLE`] (`TUIPO_ENGINE_IDLE_SECS`) → `Stopped`. After
//! [`RECYCLE_AFTER`] requests the engine restarts at the next `Boundary`
//! (Enter), which caps harper's LRU caches — over a long session they
//! otherwise add ~70 MB. If the engine can't be spawned, speaks another
//! protocol, or keeps dying, the worker falls back to running harper on
//! this thread (`TUIPO_ENGINE=inprocess` forces that): the old memory
//! profile, but still off the stdin path.

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::debug::DebugLog;
use crate::dict::CustomDict;
use crate::engine::{EngineEvent, EngineProcess};
use crate::event::{InputEvent, RenderEvent};
use crate::spell::{SpellChecker, SpellIssue};

/// Name of the worker thread. The panic hook keys on it: a panic here is
/// contained (the in-process checker runs under `catch_unwind`), so it
/// must not take the terminal out of raw mode or print over the screen.
pub const WORKER_THREAD_NAME: &str = "tuipo-lint";

/// Stop the engine after this long without the buffer changing.
const DEFAULT_IDLE: Duration = Duration::from_secs(60);

/// Restart the engine at the next `Boundary` once it has answered this
/// many requests, to drop harper's caches. Measured on typo-heavy typing:
/// ~140 MB fresh, ~210 MB after ~1,100 checks.
const RECYCLE_AFTER: u64 = 1000;

/// An engine that hasn't said hello by now is presumed wedged. harper
/// loads in ~0.2 s in release builds and ~2.5 s in debug builds.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// An engine still on one request after this long is presumed wedged.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Unexpected engine failures in a row before linting moves in-process.
const MAX_CRASHES: u32 = 3;

/// Buffers bigger than this aren't linted (a pasted log file, say):
/// harper would take seconds, and nobody reads underlines in a wall of
/// pasted text.
const MAX_LINT_BYTES: usize = 256 * 1024;

fn lintable(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= MAX_LINT_BYTES
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineMode {
    /// harper runs in a `tuipo __engine` child process. The default.
    Subprocess,
    /// harper runs on the worker thread (`TUIPO_ENGINE=inprocess`).
    InProcess,
}

#[derive(Clone, Copy, Debug)]
pub struct WorkerConfig {
    pub mode: EngineMode,
    /// Stop the engine after this long without the buffer changing.
    pub idle: Duration,
}

impl WorkerConfig {
    pub fn from_env() -> Self {
        let mode = match std::env::var("TUIPO_ENGINE") {
            Ok(v) if v.trim().eq_ignore_ascii_case("inprocess") => EngineMode::InProcess,
            _ => EngineMode::Subprocess,
        };
        let idle = std::env::var("TUIPO_ENGINE_IDLE_SECS")
            .ok()
            .and_then(|s| parse_idle(&s))
            .unwrap_or(DEFAULT_IDLE);
        Self { mode, idle }
    }
}

/// `TUIPO_ENGINE_IDLE_SECS`: positive seconds, fractions allowed.
fn parse_idle(s: &str) -> Option<Duration> {
    let secs = s.trim().parse::<f64>().ok()?;
    Duration::try_from_secs_f64(secs)
        .ok()
        .filter(|d| !d.is_zero())
}

/// State shared by the stdin pump (through [`LintHandle`]) and the worker.
struct Shared {
    state: Mutex<SharedState>,
    published: Condvar,
    /// False while `tuipo off` is in effect (see [`LintSwitch`]).
    enabled: AtomicBool,
}

#[derive(Default)]
struct SharedState {
    /// Bumped at every input boundary. Results requested in an older
    /// epoch are never published.
    epoch: u64,
    /// The last snapshot sent to the render loop in the current epoch.
    latest: Option<Snapshot>,
}

struct Snapshot {
    text: String,
    issues: Vec<SpellIssue>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, SharedState> {
        // Two plain fields, each written in a single assignment: a panic
        // elsewhere can't leave them torn, so ignore poisoning.
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The stdin pump's side of the worker.
pub struct LintHandle {
    tx: Sender<WorkerMsg>,
    shared: Arc<Shared>,
    render_tx: Sender<RenderEvent>,
}

impl LintHandle {
    /// Start the worker. `custom` is the user's dictionary; its words are
    /// filtered out of every result.
    pub fn spawn(render_tx: Sender<RenderEvent>, custom: CustomDict) -> Self {
        Self::spawn_with(render_tx, custom, WorkerConfig::from_env())
    }

    pub fn spawn_with(
        render_tx: Sender<RenderEvent>,
        custom: CustomDict,
        config: WorkerConfig,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            state: Mutex::new(SharedState::default()),
            published: Condvar::new(),
            enabled: AtomicBool::new(true),
        });
        let worker = {
            let tx = tx.clone();
            let shared = Arc::clone(&shared);
            let render_tx = render_tx.clone();
            move || Worker::new(rx, tx, shared, render_tx, custom, config).run()
        };
        // Without a worker thread lints never arrive, but passthrough is
        // unaffected — exactly the paint-off experience.
        let _ = thread::Builder::new()
            .name(WORKER_THREAD_NAME.into())
            .spawn(worker);
        Self {
            tx,
            shared,
            render_tx,
        }
    }

    /// A handle the render loop uses to pause linting while `tuipo off`
    /// is in effect.
    pub fn switch(&self) -> LintSwitch {
        LintSwitch {
            tx: self.tx.clone(),
            shared: Arc::clone(&self.shared),
        }
    }

    /// False while linting is paused (`tuipo off`).
    pub fn enabled(&self) -> bool {
        self.shared.enabled.load(Ordering::Relaxed)
    }

    /// The buffer changed: new text and/or cursor (`cursor` in chars).
    /// Never blocks. Ignored while paused.
    pub fn update(&self, text: &str, cursor: usize) {
        if !self.enabled() {
            return;
        }
        let epoch = self.shared.lock().epoch;
        let _ = self.tx.send(WorkerMsg::Update {
            epoch,
            text: text.to_owned(),
            cursor,
        });
    }

    /// Input boundary (Enter / Esc / Ctrl-C / Ctrl-D): emits
    /// `InputEvent::Boundary` to the render loop. Use this rather than
    /// sending `Boundary` directly: the epoch bump and the send happen
    /// under the lock `Worker::publish` holds, which is what keeps a late
    /// result for the old line from landing after the boundary.
    pub fn boundary(&self) {
        {
            let mut state = self.shared.lock();
            state.epoch = state.epoch.wrapping_add(1);
            state.latest = None;
            let _ = self
                .render_tx
                .send(RenderEvent::Input(InputEvent::Boundary));
        }
        let _ = self.tx.send(WorkerMsg::Boundary);
    }

    /// Issues for exactly `text`, once the worker has published them.
    /// Waits up to `timeout` — only noticeable while the engine is
    /// starting or busy — and returns `None` if they don't arrive. The
    /// caller must already have sent `text` through [`Self::update`].
    pub fn issues_for(&self, text: &str, timeout: Duration) -> Option<Vec<SpellIssue>> {
        if !self.enabled() || !lintable(text) {
            return Some(Vec::new());
        }
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.lock();
        loop {
            if let Some(snap) = &state.latest
                && snap.text == text
            {
                return Some(snap.issues.clone());
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            if left.is_zero() {
                return None;
            }
            state = match self.shared.published.wait_timeout(state, left) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }
}

impl Drop for LintHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(WorkerMsg::Shutdown);
    }
}

/// Pauses and resumes linting from another thread — the render loop, which
/// watches the `tuipo off` switch. Paused: buffer changes aren't linted,
/// Tab-fix and the picker see no lints, and the engine is stopped so its
/// memory goes back to the system right away.
#[derive(Clone)]
pub struct LintSwitch {
    tx: Sender<WorkerMsg>,
    shared: Arc<Shared>,
}

impl LintSwitch {
    pub fn set(&self, on: bool) {
        if self.shared.enabled.swap(on, Ordering::Relaxed) != on {
            let _ = self.tx.send(WorkerMsg::Enabled(on));
        }
    }
}

enum WorkerMsg {
    Update {
        epoch: u64,
        text: String,
        cursor: usize,
    },
    Boundary,
    Engine {
        generation: u64,
        event: EngineEvent,
    },
    /// Linting resumed (`true`) or paused (`false`) — see [`LintSwitch`].
    Enabled(bool),
    Shutdown,
}

/// The newest buffer state the worker hasn't finished serving.
struct Want {
    epoch: u64,
    text: String,
    cursor: usize,
}

/// What the worker last sent to the render loop.
struct Published {
    epoch: u64,
    text: String,
    issues: Vec<SpellIssue>,
    cursor: usize,
}

struct InFlight {
    id: u64,
    epoch: u64,
    text: String,
    sent_at: Instant,
}

enum Engine {
    Stopped,
    Starting {
        proc: EngineProcess,
        generation: u64,
        since: Instant,
    },
    Ready {
        proc: EngineProcess,
        generation: u64,
        in_flight: Option<InFlight>,
    },
    /// Fallback: harper on this thread, built on first use.
    InProcess(Option<SpellChecker>),
}

struct Worker {
    rx: Receiver<WorkerMsg>,
    /// Cloned into each engine's reader callback.
    tx: Sender<WorkerMsg>,
    shared: Arc<Shared>,
    render_tx: Sender<RenderEvent>,
    custom: CustomDict,
    config: WorkerConfig,
    debug: DebugLog,
    want: Option<Want>,
    published: Option<Published>,
    engine: Engine,
    /// Generation of the most recently spawned engine; events from older
    /// engines are ignored.
    generation: u64,
    next_id: u64,
    /// Requests the current engine has answered.
    served: u64,
    /// Consecutive engine failures.
    failures: u32,
    retry_at: Option<Instant>,
    /// Text the engine died or hung on. Not resent, so one input that
    /// trips a harper bug can't respawn-loop the engine.
    poisoned: Option<String>,
    last_activity: Instant,
}

/// What `pump` should do next with the current `want`.
enum Next {
    Spawn,
    Wait,
    Send,
    CheckInProcess,
}

/// Which timer, if any, has fired.
enum Timer {
    StartupTimeout,
    RequestTimeout,
    Idle,
}

impl Worker {
    fn new(
        rx: Receiver<WorkerMsg>,
        tx: Sender<WorkerMsg>,
        shared: Arc<Shared>,
        render_tx: Sender<RenderEvent>,
        custom: CustomDict,
        config: WorkerConfig,
    ) -> Self {
        let worker = Self {
            rx,
            tx,
            shared,
            render_tx,
            custom,
            config,
            debug: DebugLog::from_env(),
            want: None,
            published: None,
            engine: Engine::Stopped,
            generation: 0,
            next_id: 1,
            served: 0,
            failures: 0,
            retry_at: None,
            poisoned: None,
            last_activity: Instant::now(),
        };
        if config.mode == EngineMode::InProcess {
            worker.log(format_args!("in-process mode (TUIPO_ENGINE=inprocess)"));
        }
        worker
    }

    fn run(mut self) {
        loop {
            let first = match self.next_deadline() {
                Some(at) => match self.rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                    Ok(msg) => Some(msg),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match self.rx.recv() {
                    Ok(msg) => Some(msg),
                    Err(_) => break,
                },
            };
            // Drain everything queued so a burst of keystrokes collapses
            // into one request for the newest text.
            let mut next = first;
            while let Some(msg) = next {
                if matches!(msg, WorkerMsg::Shutdown) {
                    self.stop_engine("shutdown");
                    return;
                }
                self.handle(msg);
                next = self.rx.try_recv().ok();
            }
            self.check_timers();
            self.pump();
        }
        self.stop_engine("shutdown");
    }

    fn handle(&mut self, msg: WorkerMsg) {
        match msg {
            WorkerMsg::Update {
                epoch,
                text,
                cursor,
            } => {
                // An update sent just before a pause can arrive after it.
                if !self.shared.enabled.load(Ordering::Relaxed) {
                    return;
                }
                self.last_activity = Instant::now();
                self.want = Some(Want {
                    epoch,
                    text,
                    cursor,
                });
            }
            WorkerMsg::Boundary => {
                self.want = None;
                self.published = None;
                let idle_engine = matches!(self.engine, Engine::Ready { in_flight: None, .. });
                if idle_engine && self.served >= RECYCLE_AFTER {
                    self.stop_engine("recycle");
                }
            }
            WorkerMsg::Engine { generation, event } => self.on_engine_event(generation, event),
            WorkerMsg::Enabled(true) => {}
            WorkerMsg::Enabled(false) => {
                self.want = None;
                self.published = None;
                self.stop_engine("off");
            }
            WorkerMsg::Shutdown => {}
        }
    }

    /// The soonest moment a timer needs attention, if any.
    fn next_deadline(&self) -> Option<Instant> {
        match &self.engine {
            Engine::Starting { since, .. } => Some(*since + STARTUP_TIMEOUT),
            Engine::Ready {
                in_flight: Some(f), ..
            } => Some(f.sent_at + REQUEST_TIMEOUT),
            Engine::Ready { in_flight: None, .. } => Some(self.last_activity + self.config.idle),
            Engine::Stopped if self.want.is_some() => self.retry_at,
            Engine::Stopped | Engine::InProcess(_) => None,
        }
    }

    fn check_timers(&mut self) {
        let now = Instant::now();
        let fired = match &self.engine {
            Engine::Starting { since, .. } if now >= *since + STARTUP_TIMEOUT => {
                Some(Timer::StartupTimeout)
            }
            Engine::Ready {
                in_flight: Some(f), ..
            } if now >= f.sent_at + REQUEST_TIMEOUT => Some(Timer::RequestTimeout),
            Engine::Ready { in_flight: None, .. }
                if self.want.is_none() && now >= self.last_activity + self.config.idle =>
            {
                Some(Timer::Idle)
            }
            _ => None,
        };
        match fired {
            Some(Timer::StartupTimeout) => self.engine_failed("startup timed out"),
            Some(Timer::RequestTimeout) => self.engine_failed("request timed out"),
            Some(Timer::Idle) => self.stop_engine("idle"),
            None => {}
        }
    }

    /// Move the current `want` forward: publish it from cache, send it to
    /// the engine, start an engine for it, or check it in-process.
    fn pump(&mut self) {
        loop {
            let Some(want) = &self.want else {
                return;
            };
            // Same text as the last publish: only the cursor moved.
            // Re-emit with the new cursor (the picker's hover gate tracks
            // it) without an engine round-trip.
            if let Some(p) = &self.published
                && p.epoch == want.epoch
                && p.text == want.text
            {
                let moved = (p.cursor != want.cursor).then(|| p.issues.clone());
                let Some(want) = self.want.take() else { return };
                if let Some(issues) = moved {
                    self.publish(want, issues);
                }
                return;
            }
            if !lintable(&want.text) || self.poisoned.as_deref() == Some(want.text.as_str()) {
                let Some(want) = self.want.take() else { return };
                self.publish(want, Vec::new());
                return;
            }
            let next = match &self.engine {
                Engine::Stopped => Next::Spawn,
                Engine::Starting { .. } | Engine::Ready { in_flight: Some(_), .. } => Next::Wait,
                Engine::Ready { in_flight: None, .. } => Next::Send,
                Engine::InProcess(_) => Next::CheckInProcess,
            };
            match next {
                Next::Wait => return,
                Next::Send => {
                    self.send_want();
                    return;
                }
                Next::CheckInProcess => {
                    self.check_in_process();
                    return;
                }
                Next::Spawn => {
                    if self.retry_at.is_some_and(|at| Instant::now() < at) {
                        return;
                    }
                    self.retry_at = None;
                    self.start_engine();
                    // A failed spawn falls back to in-process: go round
                    // again and check the text right away.
                    if !matches!(self.engine, Engine::InProcess(_)) {
                        return;
                    }
                }
            }
        }
    }

    fn send_want(&mut self) {
        let Some(want) = &self.want else { return };
        let Engine::Ready {
            proc, in_flight, ..
        } = &mut self.engine
        else {
            return;
        };
        let id = self.next_id;
        self.next_id += 1;
        if proc.send(id, &want.text).is_ok() {
            *in_flight = Some(InFlight {
                id,
                epoch: want.epoch,
                text: want.text.clone(),
                sent_at: Instant::now(),
            });
        } else {
            self.engine_failed("request write failed");
        }
    }

    fn check_in_process(&mut self) {
        let Some(want) = self.want.take() else { return };
        let Engine::InProcess(slot) = &mut self.engine else {
            return;
        };
        let checker = slot.get_or_insert_with(|| SpellChecker::with_custom(CustomDict::empty()));
        // A harper panic costs this one result, not the worker.
        let issues = catch_unwind(AssertUnwindSafe(|| checker.check(&want.text))).unwrap_or_default();
        let issues = without_custom_words(&self.custom, issues);
        self.publish(want, issues);
    }

    fn on_engine_event(&mut self, generation: u64, event: EngineEvent) {
        let current = matches!(
            &self.engine,
            Engine::Starting { generation: g, .. } | Engine::Ready { generation: g, .. } if *g == generation
        );
        if !current {
            // A stopped engine's last words.
            return;
        }
        match event {
            EngineEvent::Ready => {
                if !matches!(self.engine, Engine::Starting { .. }) {
                    return;
                }
                if let Engine::Starting { proc, generation, since } =
                    std::mem::replace(&mut self.engine, Engine::Stopped)
                {
                    self.log(format_args!(
                        "ready pid={} in {}ms",
                        proc.pid(),
                        since.elapsed().as_millis()
                    ));
                    self.engine = Engine::Ready {
                        proc,
                        generation,
                        in_flight: None,
                    };
                }
            }
            EngineEvent::Reply { id, issues } => self.on_reply(id, issues),
            EngineEvent::Incompatible(reason) => {
                self.kill_engine();
                self.fall_back(&format!("incompatible engine: {reason}"));
            }
            EngineEvent::Exited => self.engine_failed("exited unexpectedly"),
        }
    }

    fn on_reply(&mut self, id: u64, issues: Vec<SpellIssue>) {
        let Engine::Ready { in_flight, .. } = &mut self.engine else {
            return;
        };
        let Some(done) = in_flight.take_if(|f| f.id == id) else {
            return;
        };
        self.served += 1;
        self.failures = 0;
        // Publish only if this is still the text the buffer holds; if it
        // moved on (or a boundary cleared it), pump() sends the newer text.
        let Some(want) = self
            .want
            .take_if(|w| w.epoch == done.epoch && w.text == done.text)
        else {
            return;
        };
        let issues = without_custom_words(&self.custom, issues);
        self.publish(want, issues);
    }

    /// Send a snapshot to the render loop and wake `issues_for` waiters,
    /// unless a boundary has happened since it was requested.
    fn publish(&mut self, want: Want, issues: Vec<SpellIssue>) {
        let Want {
            epoch,
            text,
            cursor,
        } = want;
        {
            let mut state = self.shared.lock();
            if state.epoch != epoch {
                return;
            }
            state.latest = Some(Snapshot {
                text: text.clone(),
                issues: issues.clone(),
            });
            let _ = self.render_tx.send(RenderEvent::Input(InputEvent::Lints {
                issues: issues.clone(),
                buffer_chars: text.chars().count(),
                buffer_text: text.clone(),
                buffer_cursor: cursor,
            }));
        }
        self.shared.published.notify_all();
        if self.debug.enabled() {
            self.debug.log_lints(&text, &issues);
        }
        self.published = Some(Published {
            epoch,
            text,
            issues,
            cursor,
        });
    }

    fn start_engine(&mut self) {
        if self.config.mode == EngineMode::InProcess {
            self.engine = Engine::InProcess(None);
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        let tx = self.tx.clone();
        let on_event = move |event| {
            let _ = tx.send(WorkerMsg::Engine { generation, event });
        };
        match EngineProcess::spawn(on_event) {
            Ok(proc) => {
                self.log(format_args!("spawn pid={}", proc.pid()));
                self.served = 0;
                self.engine = Engine::Starting {
                    proc,
                    generation,
                    since: Instant::now(),
                };
            }
            Err(err) => self.fall_back(&format!("spawn failed: {err}")),
        }
    }

    /// Planned stop: let the engine exit on EOF.
    fn stop_engine(&mut self, reason: &str) {
        match std::mem::replace(&mut self.engine, Engine::Stopped) {
            Engine::Starting { proc, .. } | Engine::Ready { proc, .. } => {
                self.log(format_args!(
                    "stop reason={reason} pid={} served={}",
                    proc.pid(),
                    self.served
                ));
                proc.shutdown();
            }
            other => self.engine = other,
        }
        self.served = 0;
    }

    fn kill_engine(&mut self) {
        match std::mem::replace(&mut self.engine, Engine::Stopped) {
            Engine::Starting { proc, .. } | Engine::Ready { proc, .. } => proc.kill(),
            other => self.engine = other,
        }
        self.served = 0;
    }

    /// The engine died, hung, or stopped accepting requests. Kill it,
    /// remember what it was working on, and either back off before the
    /// next spawn or give up on subprocesses for this session.
    fn engine_failed(&mut self, what: &str) {
        let (pid, in_flight) = match &self.engine {
            Engine::Starting { proc, .. } => (proc.pid(), None),
            Engine::Ready {
                proc, in_flight, ..
            } => (proc.pid(), in_flight.as_ref().map(|f| f.text.clone())),
            Engine::Stopped | Engine::InProcess(_) => return,
        };
        self.kill_engine();
        if let Some(text) = in_flight {
            // Keep serving the buffer if it still holds that text: the
            // poison check in pump() publishes it as clean.
            self.poisoned = Some(text);
        }
        self.failures += 1;
        self.log(format_args!(
            "{what} pid={pid} (failure {}/{MAX_CRASHES})",
            self.failures
        ));
        if self.failures >= MAX_CRASHES {
            self.fall_back("engine keeps failing");
        } else {
            let backoff = Duration::from_millis(500) * 2u32.pow(self.failures - 1);
            self.retry_at = Some(Instant::now() + backoff);
        }
    }

    fn fall_back(&mut self, reason: &str) {
        self.log(format_args!("falling back to in-process linting: {reason}"));
        self.engine = Engine::InProcess(None);
    }

    fn log(&self, args: fmt::Arguments<'_>) {
        if self.debug.enabled() {
            self.debug.log_engine(args);
        }
    }
}

fn without_custom_words(custom: &CustomDict, mut issues: Vec<SpellIssue>) -> Vec<SpellIssue> {
    issues.retain(|i| !custom.contains(&i.word));
    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Debug-build harper takes a few seconds to load the first time a
    /// test process touches it; later loads reuse its statics.
    const HARPER_WAIT: Duration = Duration::from_secs(60);

    fn in_process() -> WorkerConfig {
        WorkerConfig {
            mode: EngineMode::InProcess,
            idle: DEFAULT_IDLE,
        }
    }

    fn spawn(custom: CustomDict) -> (LintHandle, Receiver<RenderEvent>) {
        let (render_tx, render_rx) = mpsc::channel();
        (LintHandle::spawn_with(render_tx, custom, in_process()), render_rx)
    }

    /// Next `Lints` event, skipping anything else.
    fn next_lints(rx: &Receiver<RenderEvent>, wait: Duration) -> Option<InputEvent> {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            match rx.recv_timeout(left) {
                Ok(RenderEvent::Input(ev @ InputEvent::Lints { .. })) => return Some(ev),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }

    fn words(issues: &[SpellIssue]) -> Vec<&str> {
        issues.iter().map(|i| i.word.as_str()).collect()
    }

    #[test]
    fn update_publishes_a_snapshot_for_exactly_that_text() {
        let (handle, rx) = spawn(CustomDict::empty());
        handle.update("teh cat", 7);
        let Some(InputEvent::Lints {
            issues,
            buffer_chars,
            buffer_text,
            buffer_cursor,
        }) = next_lints(&rx, HARPER_WAIT)
        else {
            panic!("no Lints event");
        };
        assert_eq!(buffer_text, "teh cat");
        assert_eq!(buffer_chars, 7);
        assert_eq!(buffer_cursor, 7);
        assert!(words(&issues).contains(&"teh"), "{issues:?}");
    }

    #[test]
    fn issues_for_waits_for_the_live_text_only() {
        let (handle, _rx) = spawn(CustomDict::empty());
        handle.update("teh dog", 7);
        let issues = handle
            .issues_for("teh dog", HARPER_WAIT)
            .expect("snapshot for the live text");
        assert!(words(&issues).contains(&"teh"), "{issues:?}");
        // Text the worker was never asked about must time out, not return
        // another text's snapshot.
        assert!(handle.issues_for("teh dog barks", Duration::from_millis(50)).is_none());
    }

    #[test]
    fn issues_for_blank_or_oversized_text_is_immediate() {
        let (handle, _rx) = spawn(CustomDict::empty());
        assert_eq!(handle.issues_for("   ", Duration::ZERO).map(|v| v.len()), Some(0));
        let huge = "teh ".repeat(MAX_LINT_BYTES / 4 + 1);
        assert_eq!(handle.issues_for(&huge, Duration::ZERO).map(|v| v.len()), Some(0));
    }

    #[test]
    fn cursor_only_update_reemits_the_cached_snapshot() {
        let (handle, rx) = spawn(CustomDict::empty());
        handle.update("teh fox", 7);
        next_lints(&rx, HARPER_WAIT).expect("first snapshot");
        handle.update("teh fox", 2);
        let Some(InputEvent::Lints {
            issues,
            buffer_text,
            buffer_cursor,
            ..
        }) = next_lints(&rx, Duration::from_secs(5))
        else {
            panic!("cursor move produced no Lints event");
        };
        assert_eq!(buffer_text, "teh fox");
        assert_eq!(buffer_cursor, 2);
        assert!(words(&issues).contains(&"teh"), "{issues:?}");
    }

    #[test]
    fn nothing_from_before_a_boundary_arrives_after_it() {
        let (handle, rx) = spawn(CustomDict::empty());
        // Warm harper so the race below is about ordering, not load time.
        handle.update("warm", 4);
        next_lints(&rx, HARPER_WAIT).expect("warm-up snapshot");

        for round in 0..20 {
            let line = format!("teh line number {round}");
            handle.update(&line, line.len());
            handle.boundary();
            // Drain until the worker has gone quiet; after the Boundary
            // event, no Lints for the submitted line may appear.
            let mut seen_boundary = false;
            while let Ok(ev) = rx.recv_timeout(Duration::from_millis(200)) {
                match ev {
                    RenderEvent::Input(InputEvent::Boundary) => seen_boundary = true,
                    RenderEvent::Input(InputEvent::Lints { buffer_text, .. }) => assert!(
                        !seen_boundary,
                        "round {round}: Lints for {buffer_text:?} arrived after Boundary"
                    ),
                    _ => {}
                }
            }
            assert!(seen_boundary, "round {round}: Boundary never reached the render loop");
        }
        // And the next line is served normally.
        handle.update("new teh line", 12);
        let issues = handle.issues_for("new teh line", HARPER_WAIT).expect("post-boundary snapshot");
        assert!(words(&issues).contains(&"teh"), "{issues:?}");
    }

    #[test]
    fn custom_dictionary_words_are_filtered_from_results() {
        let (handle, _rx) = spawn(CustomDict::from_text("teh\n"));
        handle.update("teh wrold", 9);
        let issues = handle.issues_for("teh wrold", HARPER_WAIT).expect("snapshot");
        assert!(!words(&issues).contains(&"teh"), "custom word leaked: {issues:?}");
        assert!(words(&issues).contains(&"wrold"), "{issues:?}");
    }

    #[test]
    fn blank_text_publishes_an_empty_snapshot() {
        let (handle, rx) = spawn(CustomDict::empty());
        handle.update("  ", 2);
        let Some(InputEvent::Lints {
            issues,
            buffer_text,
            ..
        }) = next_lints(&rx, Duration::from_secs(5))
        else {
            panic!("blank text produced no Lints event");
        };
        assert_eq!(buffer_text, "  ");
        assert!(issues.is_empty());
    }

    #[test]
    fn paused_worker_lints_nothing_and_resumes_on_request() {
        let (handle, rx) = spawn(CustomDict::empty());
        let switch = handle.switch();
        switch.set(false);
        assert!(!handle.enabled());
        handle.update("teh cat", 7);
        assert!(
            next_lints(&rx, Duration::from_millis(300)).is_none(),
            "paused worker published lints"
        );
        // Tab-fix must not wait on a paused worker.
        assert_eq!(handle.issues_for("teh cat", Duration::ZERO).map(|v| v.len()), Some(0));

        switch.set(true);
        handle.update("teh cat", 7);
        let issues = handle.issues_for("teh cat", HARPER_WAIT).expect("resumed snapshot");
        assert!(words(&issues).contains(&"teh"), "{issues:?}");
    }

    #[test]
    fn idle_setting_parses_fractional_seconds_and_ignores_junk() {
        // The parser is tested directly rather than through `from_env`, so
        // the test doesn't race others over process-wide env vars.
        assert_eq!(parse_idle("0.5"), Some(Duration::from_millis(500)));
        assert_eq!(parse_idle(" 90 "), Some(Duration::from_secs(90)));
        assert_eq!(parse_idle("0"), None);
        assert_eq!(parse_idle("-3"), None);
        assert_eq!(parse_idle("soon"), None);
        assert_eq!(parse_idle("1e30"), None);
    }
}
