//! Out-of-process spell engine.
//!
//! harper-core builds ~100 MB of dictionary data into private,
//! process-lifetime statics the first time anything touches it: an
//! orphaned `MutableDictionary` copy that nothing reads after startup
//! (upstream harper#4071), `FstDictionary`'s own word map and word list,
//! and a `TrieDictionary`. Its rules reach for those statics directly,
//! so tuipo can't hand it a leaner dictionary, nothing can free them while
//! the process lives, and no two processes share them. Every wrapped
//! terminal used to pay the full cost, idle or not.
//!
//! So harper runs in a child process — `tuipo __engine` — that the lint
//! worker (`lint_worker.rs`) starts on demand and stops after a quiet
//! period. Exiting is the only way to hand harper's memory back. A cold
//! start costs ~210 ms in release builds, less than it takes to type a
//! word, and the wrapped child never waits on it because the stdin pump
//! no longer runs harper at all.
//!
//! ## Wire protocol
//!
//! Length-prefixed frames over the engine's stdin/stdout: a `u32` LE
//! payload length, then the payload. Integers are little-endian; strings
//! are a `u32` length + UTF-8 bytes.
//!
//! - **Hello** (engine → tuipo, once, after harper is warm): [`MAGIC`] +
//!   `u32` [`PROTOCOL_VERSION`] + string crate version.
//! - **Request** (tuipo → engine): `u64` id + text (the rest of the frame).
//! - **Response** (engine → tuipo): `u64` id + `u32` count + that many
//!   issues (see [`encode_response`]).
//!
//! The engine is config-agnostic: it returns harper's lints after the
//! code-shape filter, and the client applies the user's custom dictionary
//! (loaded once when tuipo starts, as it always was).

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::dict::CustomDict;
use crate::spell::{IssueCategory, SpellChecker, SpellIssue};

/// Hidden subcommand that runs the engine. Not listed in `--help`.
pub const ENGINE_SUBCOMMAND: &str = "__engine";

const MAGIC: &[u8; 12] = b"TUIPO-ENGINE";

/// Bump whenever a frame layout changes. A mismatched engine (e.g. the
/// binary on disk was upgraded under a running session) is rejected and
/// the worker falls back to linting in-process. v2 added `rule`.
const PROTOCOL_VERSION: u32 = 2;

/// Sanity cap on a single frame. Requests are already capped far lower by
/// the worker (`lint_worker::MAX_LINT_BYTES`); this only stops a corrupt
/// length prefix from triggering a huge allocation.
const MAX_FRAME: usize = 64 << 20;

/// Grace period between closing the engine's stdin and killing it.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------
// Server: `tuipo __engine`
// ---------------------------------------------------------------------

/// Entry point for the `__engine` subcommand. Returns the process exit
/// code. stdin carries requests and stdout responses; stderr is wired to
/// `/dev/null` by the client so a panic can't scribble on the user's
/// raw-mode terminal.
pub fn run_server() -> i32 {
    match serve(io::stdin().lock(), io::stdout().lock()) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// Serve requests until the client closes our stdin. Generic over the
/// streams so the full loop is unit-testable in memory.
fn serve(input: impl Read, output: impl Write) -> io::Result<()> {
    let mut checker = SpellChecker::with_custom(CustomDict::empty());
    // Pull in harper's lazily-built tagger/chunker models now, so "ready"
    // means warm and the first real request doesn't pay for them.
    let _ = checker.check("warm up teh engine");
    let mut reader = BufReader::new(input);
    let mut writer = BufWriter::new(output);
    write_frame(&mut writer, &encode_hello())?;
    writer.flush()?;
    while let Some(frame) = read_frame(&mut reader)? {
        let (id, text) = decode_request(&frame)?;
        let issues = checker.check(text);
        write_frame(&mut writer, &encode_response(id, &issues))?;
        writer.flush()?;
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Client: spawn + talk to an engine process
// ---------------------------------------------------------------------

/// What the engine's reader thread reports back to the lint worker.
#[derive(Debug)]
pub enum EngineEvent {
    /// Harper is loaded and warm; requests may be sent.
    Ready,
    /// Result for request `id`.
    Reply { id: u64, issues: Vec<SpellIssue> },
    /// The engine speaks a different protocol (the binary on disk changed
    /// under us, or isn't tuipo at all). Respawning won't help.
    Incompatible(String),
    /// The engine's stdout closed or carried garbage: it exited, crashed,
    /// or was killed.
    Exited,
}

/// A running `tuipo __engine` child. Requests go out through its stdin;
/// a dedicated reader thread decodes its stdout and reports through the
/// callback given to [`EngineProcess::spawn`].
pub struct EngineProcess {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
}

impl EngineProcess {
    /// Start an engine. `on_event` runs on the reader thread for every
    /// event, ending with `Exited` or `Incompatible`.
    pub fn spawn(on_event: impl FnMut(EngineEvent) + Send + 'static) -> io::Result<Self> {
        let mut child = Command::new(engine_executable()?)
            .arg(ENGINE_SUBCOMMAND)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other("engine pipes unavailable"));
        };
        let spawned = thread::Builder::new()
            .name("tuipo-engine-io".into())
            .spawn(move || read_events(stdout, on_event));
        if let Err(err) = spawned {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.as_ref().map_or(0, Child::id)
    }

    /// Send one request. An error means the engine is gone.
    pub fn send(&mut self, id: u64, text: &str) -> io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::other("engine stdin closed"))?;
        write_frame(stdin, &encode_request(id, text))?;
        stdin.flush()
    }

    /// Ask the engine to exit (it stops at EOF on stdin) and reap it off
    /// the caller's thread, killing it if it overstays the grace period.
    pub fn shutdown(mut self) {
        drop(self.stdin.take());
        if let Some(child) = self.child.take() {
            reap_in_background(child, SHUTDOWN_GRACE);
        }
    }

    /// Kill immediately — for an engine that stopped answering.
    pub fn kill(mut self) {
        drop(self.stdin.take());
        if let Some(child) = self.child.take() {
            reap_in_background(child, Duration::ZERO);
        }
    }
}

impl Drop for EngineProcess {
    fn drop(&mut self) {
        // Safety net for paths that didn't call shutdown()/kill().
        drop(self.stdin.take());
        if let Some(child) = self.child.take() {
            reap_in_background(child, SHUTDOWN_GRACE);
        }
    }
}

/// Wait for `child` on a throwaway thread so the lint worker never blocks
/// on process teardown; kill it once `grace` has passed.
fn reap_in_background(mut child: Child, grace: Duration) {
    let reap = move || {
        let step = Duration::from_millis(10);
        let mut waited = Duration::ZERO;
        while waited < grace {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            thread::sleep(step);
            waited += step;
        }
        let _ = child.kill();
        let _ = child.wait();
    };
    // If no thread can be had, the engine still exits on EOF; left
    // unreaped, it lingers only as a zombie that holds no memory.
    let _ = thread::Builder::new()
        .name("tuipo-engine-reap".into())
        .spawn(reap);
}

/// The binary to run as the engine: ourselves.
fn engine_executable() -> io::Result<PathBuf> {
    // On Linux `/proc/self/exe` re-executes the exact image this process
    // is running, even if an upgrade has since replaced or removed the
    // file on disk.
    #[cfg(target_os = "linux")]
    {
        let proc_exe = PathBuf::from("/proc/self/exe");
        if proc_exe.exists() {
            return Ok(proc_exe);
        }
    }
    std::env::current_exe()
}

/// Reader-thread body: hello first, then responses until EOF.
fn read_events(stdout: ChildStdout, mut on_event: impl FnMut(EngineEvent)) {
    let mut reader = BufReader::new(stdout);
    let hello = match read_frame(&mut reader) {
        Ok(Some(frame)) => decode_hello(&frame),
        Ok(None) | Err(_) => {
            on_event(EngineEvent::Exited);
            return;
        }
    };
    if let Err(reason) = hello {
        on_event(EngineEvent::Incompatible(reason));
        return;
    }
    on_event(EngineEvent::Ready);
    while let Ok(Some(frame)) = read_frame(&mut reader) {
        match decode_response(&frame) {
            Ok((id, issues)) => on_event(EngineEvent::Reply { id, issues }),
            Err(_) => break,
        }
    }
    on_event(EngineEvent::Exited);
}

// ---------------------------------------------------------------------
// Framing + encoding
// ---------------------------------------------------------------------

fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|&n| n as usize <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)
}

/// Read one frame. `Ok(None)` on EOF at a frame boundary.
fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some(payload))
}

#[derive(Default)]
struct Enc(Vec<u8>);

impl Enc {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn usize(&mut self, v: usize) {
        self.u64(v as u64);
    }
    fn str(&mut self, s: &str) {
        // Lengths are bounded by MAX_FRAME, far below u32::MAX.
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }
}

struct Dec<'a>(&'a [u8]);

impl<'a> Dec<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(invalid("truncated frame"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> io::Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> io::Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }
    fn usize(&mut self) -> io::Result<usize> {
        usize::try_from(self.u64()?).map_err(|_| invalid("offset out of range"))
    }
    fn str(&mut self) -> io::Result<&'a str> {
        let n = self.u32()? as usize;
        std::str::from_utf8(self.take(n)?).map_err(|_| invalid("invalid UTF-8"))
    }
    fn rest_str(&mut self) -> io::Result<&'a str> {
        let rest = std::mem::take(&mut self.0);
        std::str::from_utf8(rest).map_err(|_| invalid("invalid UTF-8"))
    }
    fn finish(&self) -> io::Result<()> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(invalid("trailing bytes in frame"))
        }
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn encode_hello() -> Vec<u8> {
    let mut e = Enc::default();
    e.0.extend_from_slice(MAGIC);
    e.u32(PROTOCOL_VERSION);
    e.str(env!("CARGO_PKG_VERSION"));
    e.0
}

fn decode_hello(frame: &[u8]) -> Result<(), String> {
    let mut d = Dec(frame);
    match d.take(MAGIC.len()) {
        Ok(magic) if magic == MAGIC => {}
        _ => return Err("not a tuipo engine".into()),
    }
    let version = d.u32().map_err(|e| e.to_string())?;
    if version != PROTOCOL_VERSION {
        let build = d.str().unwrap_or("?");
        return Err(format!(
            "engine protocol {version} (tuipo {build}), expected {PROTOCOL_VERSION}"
        ));
    }
    Ok(())
}

fn encode_request(id: u64, text: &str) -> Vec<u8> {
    let mut e = Enc::default();
    e.u64(id);
    e.0.extend_from_slice(text.as_bytes());
    e.0
}

fn decode_request(frame: &[u8]) -> io::Result<(u64, &str)> {
    let mut d = Dec(frame);
    let id = d.u64()?;
    let text = d.rest_str()?;
    Ok((id, text))
}

/// Per issue: byte_start, byte_end, char_start, char_end (`u64` each),
/// category (`u8`), priority (`u8`), word, message, suggestion count
/// (`u32`), suggestions, rule.
fn encode_response(id: u64, issues: &[SpellIssue]) -> Vec<u8> {
    let mut e = Enc::default();
    e.u64(id);
    e.u32(issues.len() as u32);
    for issue in issues {
        e.usize(issue.byte_start);
        e.usize(issue.byte_end);
        e.usize(issue.char_start);
        e.usize(issue.char_end);
        e.u8(category_to_wire(issue.category));
        e.u8(issue.priority);
        e.str(&issue.word);
        e.str(&issue.message);
        e.u32(issue.suggestions.len() as u32);
        for s in &issue.suggestions {
            e.str(s);
        }
        e.str(&issue.rule);
    }
    e.0
}

fn decode_response(frame: &[u8]) -> io::Result<(u64, Vec<SpellIssue>)> {
    let mut d = Dec(frame);
    let id = d.u64()?;
    let count = d.u32()? as usize;
    // Don't trust `count` for the allocation size: each issue takes at
    // least 50 bytes on the wire, which bounds what the frame can hold.
    let mut issues = Vec::with_capacity(count.min(frame.len() / 50));
    for _ in 0..count {
        let byte_start = d.usize()?;
        let byte_end = d.usize()?;
        let char_start = d.usize()?;
        let char_end = d.usize()?;
        let category = category_from_wire(d.u8()?);
        let priority = d.u8()?;
        let word = d.str()?.to_string();
        let message = d.str()?.to_string();
        let n = d.u32()? as usize;
        let mut suggestions = Vec::with_capacity(n.min(16));
        for _ in 0..n {
            suggestions.push(d.str()?.to_string());
        }
        let rule = d.str()?.to_string();
        issues.push(SpellIssue {
            byte_start,
            byte_end,
            char_start,
            char_end,
            word,
            message,
            suggestions,
            category,
            priority,
            rule,
        });
    }
    d.finish()?;
    Ok((id, issues))
}

fn category_to_wire(c: IssueCategory) -> u8 {
    match c {
        IssueCategory::Spelling => 0,
        IssueCategory::Grammar => 1,
        IssueCategory::Style => 2,
        IssueCategory::Other => 3,
    }
}

fn category_from_wire(b: u8) -> IssueCategory {
    match b {
        0 => IssueCategory::Spelling,
        1 => IssueCategory::Grammar,
        2 => IssueCategory::Style,
        _ => IssueCategory::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn issue(word: &str, char_start: usize, category: IssueCategory) -> SpellIssue {
        SpellIssue {
            byte_start: char_start + 1,
            byte_end: char_start + 1 + word.len(),
            char_start,
            char_end: char_start + word.chars().count(),
            word: word.into(),
            message: format!("Did you mean to spell `{word}` this way?"),
            suggestions: vec!["the".into(), "tea".into(), "naïve".into()],
            category,
            priority: 63,
            rule: format!("Rule{char_start}"),
        }
    }

    fn frames(payloads: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in payloads {
            write_frame(&mut out, p).unwrap();
        }
        out
    }

    #[test]
    fn response_round_trips_every_field() {
        let issues = vec![
            issue("teh", 6, IssueCategory::Spelling),
            issue("for all intensive purposes", 10, IssueCategory::Grammar),
            issue("ünïcödé", 40, IssueCategory::Style),
            issue("x", 99, IssueCategory::Other),
        ];
        let (id, back) = decode_response(&encode_response(42, &issues)).unwrap();
        assert_eq!(id, 42);
        assert_eq!(back.len(), issues.len());
        for (a, b) in issues.iter().zip(&back) {
            assert_eq!(a.byte_start, b.byte_start);
            assert_eq!(a.byte_end, b.byte_end);
            assert_eq!(a.char_start, b.char_start);
            assert_eq!(a.char_end, b.char_end);
            assert_eq!(a.word, b.word);
            assert_eq!(a.message, b.message);
            assert_eq!(a.suggestions, b.suggestions);
            assert_eq!(a.category, b.category);
            assert_eq!(a.priority, b.priority);
            assert_eq!(a.rule, b.rule);
        }
    }

    #[test]
    fn empty_response_round_trips() {
        let (id, back) = decode_response(&encode_response(7, &[])).unwrap();
        assert_eq!(id, 7);
        assert!(back.is_empty());
    }

    #[test]
    fn request_round_trips_multiline_unicode_text() {
        let text = "fix teh bug\nand the naïve café — ✓";
        let frame = encode_request(u64::MAX, text);
        assert_eq!(decode_request(&frame).unwrap(), (u64::MAX, text));
    }

    #[test]
    fn truncated_response_is_an_error_not_a_panic() {
        let full = encode_response(1, &[issue("teh", 0, IssueCategory::Spelling)]);
        for cut in 0..full.len() {
            assert!(decode_response(&full[..cut]).is_err(), "cut at {cut} decoded");
        }
    }

    #[test]
    fn huge_issue_count_does_not_preallocate() {
        // A corrupt count must fail on the missing bytes, not attempt a
        // multi-gigabyte allocation up front.
        let mut e = Enc::default();
        e.u64(1);
        e.u32(u32::MAX);
        assert!(decode_response(&e.0).is_err());
    }

    #[test]
    fn hello_accepts_matching_protocol() {
        assert_eq!(decode_hello(&encode_hello()), Ok(()));
    }

    #[test]
    fn hello_rejects_other_protocol_versions_and_strangers() {
        let mut e = Enc::default();
        e.0.extend_from_slice(MAGIC);
        e.u32(PROTOCOL_VERSION + 1);
        e.str("9.9.9");
        let err = decode_hello(&e.0).unwrap_err();
        assert!(err.contains("9.9.9"), "{err}");
        assert!(decode_hello(b"running 3 tests").is_err());
        assert!(decode_hello(b"").is_err());
    }

    #[test]
    fn read_frame_reports_clean_eof_and_rejects_oversize() {
        assert!(read_frame(&mut Cursor::new(Vec::new())).unwrap().is_none());
        let huge = ((MAX_FRAME + 1) as u32).to_le_bytes();
        assert!(read_frame(&mut Cursor::new(huge.to_vec())).is_err());
    }

    #[test]
    fn server_answers_requests_in_order_then_exits_on_eof() {
        let input = frames(&[
            encode_request(1, "teh cat"),
            encode_request(2, "   "),
            encode_request(3, "hello world"),
        ]);
        let mut output = Vec::new();
        serve(Cursor::new(input), &mut output).unwrap();

        let mut r = Cursor::new(output);
        let hello = read_frame(&mut r).unwrap().expect("hello frame");
        assert_eq!(decode_hello(&hello), Ok(()));

        let (id, issues) = decode_response(&read_frame(&mut r).unwrap().unwrap()).unwrap();
        assert_eq!(id, 1);
        let teh = issues
            .iter()
            .find(|i| i.word == "teh")
            .expect("engine should flag `teh`");
        assert_eq!((teh.char_start, teh.char_end), (0, 3));
        assert_eq!(teh.category, IssueCategory::Spelling);
        assert!(teh.suggestions.iter().any(|s| s == "the"));

        let (id, issues) = decode_response(&read_frame(&mut r).unwrap().unwrap()).unwrap();
        assert_eq!(id, 2);
        assert!(issues.is_empty());

        let (id, issues) = decode_response(&read_frame(&mut r).unwrap().unwrap()).unwrap();
        assert_eq!(id, 3);
        assert!(
            issues.iter().all(|i| i.category != IssueCategory::Spelling),
            "clean text flagged: {issues:?}"
        );

        assert!(read_frame(&mut r).unwrap().is_none(), "extra output after EOF");
    }

    #[test]
    fn server_ignores_the_custom_dictionary() {
        // Custom-dict filtering is the client's job (the worker applies
        // the dict tuipo loaded at startup); the engine must report the
        // raw lint so a word added to dict.txt mid-session behaves the
        // same whether or not the engine has restarted since.
        let input = frames(&[encode_request(1, "ultrathink abut it")]);
        let mut output = Vec::new();
        serve(Cursor::new(input), &mut output).unwrap();
        let mut r = Cursor::new(output);
        let _hello = read_frame(&mut r).unwrap();
        let (_, issues) = decode_response(&read_frame(&mut r).unwrap().unwrap()).unwrap();
        assert!(
            issues.iter().any(|i| i.word == "ultrathink"),
            "engine filtered a bundled-dict word: {issues:?}"
        );
    }
}
