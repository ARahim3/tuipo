//! Phase 1 integration tests for the transparent PTY wrapper.
//!
//! These run without a TTY (stdin/stdout are pipes from the test harness),
//! so raw mode is skipped and we can verify the byte-pump path end-to-end.
//! Interactive behavior (vim, resize, Ctrl-C) is covered by the manual
//! smoke-test checklist in MANUAL_TESTS.md.

use assert_cmd::Command;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

fn tuipo() -> Command {
    let mut cmd = Command::cargo_bin("tuipo").expect("binary `tuipo` not built");
    cmd.timeout(TIMEOUT);
    cmd.env("XDG_CONFIG_HOME", isolated_config_dir());
    cmd
}

/// Point the spawned `tuipo` at an empty config dir so the integration
/// tests are hermetic. Without this, the developer's real
/// `~/.config/tuipo/config.toml` leaks in: e.g. `picker = true` reroutes
/// `TUIPO_TAB_FIX=1` through the picker-engage path instead of the
/// auto-apply path, and `grammar = true` changes which lints surface —
/// either of which flakes a test depending on the machine it runs on. A
/// missing `<dir>/tuipo/config.toml` makes `Config::load` fall back to
/// the documented defaults (picker/tab_fix/grammar all off).
fn isolated_config_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("tuipo-test-noconfig");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Debug-build harper takes a few seconds to load; release takes ~0.2 s.
const ENGINE_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// How many times `needle` appears in the debug log at `path`.
fn log_count(path: &std::path::Path, needle: &str) -> usize {
    std::fs::read_to_string(path)
        .map(|log| log.matches(needle).count())
        .unwrap_or(0)
}

/// Block until the debug log at `path` holds at least `n` copies of
/// `needle`. The spell engine starts on the first keystroke, so tests
/// type one character, wait for `ready pid=`, then type the rest.
fn wait_for_log(path: &std::path::Path, needle: &str, n: usize, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if log_count(path, needle) >= n {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// The pid from the most recent `ready pid=<n>` engine line.
fn last_engine_pid(path: &std::path::Path) -> Option<u32> {
    let log = std::fs::read_to_string(path).ok()?;
    let (_, rest) = log.rsplit_once("ready pid=")?;
    rest.split_whitespace().next()?.parse().ok()
}

/// A config dir of its own, for tests that flip `tuipo off`: the shared
/// `isolated_config_dir` must never be switched off under other tests.
fn fresh_config_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tuipo-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn tuipo_switch(config_dir: &std::path::Path, which: &str) -> String {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tuipo"))
        .arg(which)
        .env("XDG_CONFIG_HOME", config_dir)
        .output()
        .expect("run tuipo on/off");
    assert!(out.status.success(), "tuipo {which} failed: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn type_slowly(stdin: &mut impl std::io::Write, text: &str) {
    for c in text.chars() {
        write!(stdin, "{c}").unwrap();
        stdin.flush().unwrap();
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn child_stdout_reaches_us() {
    let assert = tuipo().args(["--", "echo", "hello-tuipo"]).assert();
    let output = assert.get_output().clone();
    assert!(
        output.status.success(),
        "exit was not success: status={:?}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello-tuipo"),
        "stdout did not contain expected text: {stdout:?}",
    );
}

#[test]
fn exit_code_zero_propagates() {
    tuipo().args(["--", "true"]).assert().success();
}

#[test]
fn exit_code_nonzero_propagates() {
    tuipo().args(["--", "false"]).assert().failure();
}

#[test]
fn specific_exit_code_propagates() {
    let assert = tuipo()
        .args(["--", "sh", "-c", "exit 42"])
        .assert()
        .failure();
    let code = assert.get_output().status.code();
    assert_eq!(code, Some(42), "expected exit 42, got {code:?}");
}

#[test]
fn child_sees_tuipo_active_env() {
    let assert = tuipo()
        .args(["--", "sh", "-c", "printf '%s' \"${TUIPO_ACTIVE:-MISSING}\""])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stdout.contains('1'),
        "TUIPO_ACTIVE was not set in child env: {stdout:?}",
    );
}

#[test]
fn child_cwd_matches_parent() {
    // /tmp on mac symlinks to /private/tmp, so canonicalize both sides.
    let tmp = std::fs::canonicalize(std::env::temp_dir())
        .expect("temp_dir should canonicalize on this platform");
    let assert = tuipo()
        .current_dir(&tmp)
        .args(["--", "sh", "-c", "pwd"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    // In non-TTY mode, the PTY line discipline can emit a `^D\b\b` visual EOF
    // marker before the child's output (because stdin is /dev/null and the
    // pump closes its writer end on first read). Find the first absolute path
    // in the output by scanning for `/` in each CR/LF-separated chunk.
    let actual_raw = stdout
        .lines()
        .filter_map(|line| line.find('/').map(|i| line[i..].trim().to_string()))
        .find(|p| !p.is_empty())
        .unwrap_or_else(|| {
            panic!(
                "no absolute path in child output; raw bytes: {:?}",
                assert.get_output().stdout
            )
        });
    let actual_canon = std::fs::canonicalize(&actual_raw).unwrap_or_else(|e| {
        panic!("child pwd {actual_raw:?} did not resolve: {e}")
    });
    assert_eq!(
        actual_canon, tmp,
        "child pwd resolved to {} but parent cwd was {}",
        actual_canon.display(),
        tmp.display(),
    );
}

#[test]
fn missing_command_argument_is_error() {
    tuipo().assert().failure();
}

#[test]
fn unknown_binary_produces_clear_error() {
    let assert = tuipo()
        .args(["--", "definitely-not-a-real-binary-zzz-123"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.to_lowercase().contains("spawn")
            || stderr.to_lowercase().contains("not")
            || stderr.to_lowercase().contains("no such"),
        "expected a spawn-error message, got: {stderr:?}",
    );
}

#[test]
fn multibyte_output_passes_through_unchanged() {
    let assert = tuipo()
        .args(["--", "printf", "%s", "café 日本 🦀"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(stdout.contains("café"), "missing latin-1: {stdout:?}");
    assert!(stdout.contains("日本"), "missing CJK: {stdout:?}");
    assert!(stdout.contains("🦀"), "missing emoji: {stdout:?}");
}

/// End-to-end paint test. Types prose with delays, pauses, then closes
/// stdin. Asserts that the captured stdout contains the SGR underline+red
/// sequence wrapping the expected misspelling.
#[test]
fn paint_emits_underline_ansi_after_pause() {
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    // Drop the debug log to a per-test tempfile so a failure can include it.
    let log_path =
        std::env::temp_dir().join(format!("tuipo-paint-test-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log_path);

    let mut child = Command::new(env!("CARGO_BIN_EXE_tuipo"))
        .args(["--", "cat"])
        .env("TUIPO_DEBUG_LOG", &log_path)
        // Hermetic: a real `tuipo off` (or config) on the dev machine must
        // not change what this test sees.
        .env("XDG_CONFIG_HOME", isolated_config_dir())
        // Pin the underline style to PLAIN regardless of the test runner's
        // terminal — the SGR assertions below check the exact byte
        // sequence and would flake if TERM_PROGRAM-detection switched us
        // to the FANCY colon-form style.
        .env("TUIPO_PLAIN_UNDERLINE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tuipo");

    let mut stdin = child.stdin.take().unwrap();
    let ready_log = log_path.clone();
    let writer = thread::spawn(move || {
        // The first keystroke starts the spell engine; let it load.
        type_slowly(&mut stdin, "w");
        assert!(
            wait_for_log(&ready_log, "ready pid=", 1, ENGINE_READY_TIMEOUT),
            "spell engine never became ready"
        );
        // Trailing space matters: the painter skips the word being typed
        // (lint touching cursor) to avoid stale-paint cells when partial
        // words flip in and out of the lint set. The space ensures `teh`
        // is a completed word by the time the pause hits.
        type_slowly(&mut stdin, "rite teh paragprah ");
        thread::sleep(Duration::from_millis(500));
        drop(stdin);
    });

    let output = child.wait_with_output().expect("wait_with_output");
    writer.join().unwrap();
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);

    let raw = String::from_utf8_lossy(&output.stdout);
    let fail_ctx = || format!("\n--- stdout ---\n{raw:?}\n--- debug log ---\n{log}");
    // The painter emits plain `\x1b[4m` underline + `\x1b[24m` reset —
    // the only SGR forms guaranteed to render identically on every
    // terminal we care about (Apple Terminal mis-parses the colon
    // sub-parameter forms by digit-concatenation).
    assert!(
        raw.contains("\x1b[4m"),
        "missing plain-underline SGR{}",
        fail_ctx(),
    );
    assert!(
        raw.contains("\x1b[24m"),
        "missing underline-off SGR{}",
        fail_ctx(),
    );
    assert!(
        !raw.contains("\x1b[4:3m"),
        "colon-undercurl SGR leaked (breaks Apple Terminal){}",
        fail_ctx(),
    );
    assert!(
        !raw.contains("\x1b[58:"),
        "colon-underline-color SGR leaked (breaks Apple Terminal){}",
        fail_ctx(),
    );
    assert!(
        !raw.contains("\x1b[4;91m"),
        "legacy red-foreground SGR leaked{}",
        fail_ctx(),
    );
    let painted_teh = raw.contains("\x1b[4mteh\x1b[24m");
    let painted_par = raw.contains("\x1b[4mparagprah\x1b[24m");
    assert!(
        painted_teh || painted_par,
        "no expected misspelling painted with plain underline{}",
        fail_ctx(),
    );
}

/// End-to-end Tab-fix test. Types a misspelling, pauses for harper to
/// catch up, presses Tab, then verifies the child's view of the input
/// shows the correction (i.e., the replacement bytes reached the PTY).
#[test]
fn tab_replaces_most_recent_misspelling() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    let log_path = std::env::temp_dir().join(format!("tuipo-tab-test-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log_path);

    let mut child = Command::new(env!("CARGO_BIN_EXE_tuipo"))
        .args(["--", "cat"])
        .env("TUIPO_DEBUG_LOG", &log_path)
        // Tab fix is opt-in; enable for this test.
        .env("TUIPO_TAB_FIX", "1")
        // Hermetic config: ignore the developer's real ~/.config (a live
        // `picker = true` would reroute Tab through the picker-engage path
        // and this test would never reach the auto-apply branch).
        .env("XDG_CONFIG_HOME", isolated_config_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tuipo");

    let mut stdin = child.stdin.take().unwrap();
    let ready_log = log_path.clone();
    let writer = thread::spawn(move || {
        type_slowly(&mut stdin, "teh");
        // The first keystroke started the spell engine; once it's ready
        // it lints the buffer it was waiting on. (Tab-fix itself waits up
        // to a second for lints matching the live buffer — enough for a
        // release-build cold start, not a debug one.)
        assert!(
            wait_for_log(&ready_log, "ready pid=", 1, ENGINE_READY_TIMEOUT),
            "spell engine never became ready"
        );
        thread::sleep(Duration::from_millis(100));
        // Press Tab.
        stdin.write_all(b"\t").unwrap();
        stdin.flush().unwrap();
        thread::sleep(Duration::from_millis(200));
        // Then a newline so cat flushes its line.
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
        thread::sleep(Duration::from_millis(200));
        drop(stdin);
    });

    let output = child.wait_with_output().expect("wait_with_output");
    writer.join().unwrap();
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);

    let raw = String::from_utf8_lossy(&output.stdout);
    // cat will have echoed back what reached its stdin. After Tab-fix, the
    // bytes that reached the child are: t,e,h, BS,BS,BS, t,h,e, \n. cat's
    // line-buffered flush prints the FINAL state of the line — i.e., "the".
    assert!(
        raw.contains("the"),
        "expected `the` in child output\n--- stdout ---\n{raw:?}\n--- log ---\n{log}",
    );
    // Verify the FIX debug-log marker fired.
    assert!(
        log.contains("FIX"),
        "no FIX debug-log entry observed\n--- log ---\n{log}",
    );
    assert!(
        log.contains("\"teh\""),
        "FIX log didn't reference the misspelling\n--- log ---\n{log}",
    );
}

/// End-to-end paste test (regression for GH #1: paste hang). Writes a
/// multi-line block as a single chunk into a wrapped `cat`, then asserts
/// every pasted line round-trips back through the child. The fast paste
/// path in `pty::handle_paste_chunk` forwards the whole chunk before
/// running harper, so the content must arrive intact and unmangled — no
/// mid-paste boundary swallowing newlines, no dropped bytes.
#[test]
fn multiline_paste_round_trips_through_child() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    let mut child = Command::new(env!("CARGO_BIN_EXE_tuipo"))
        .args(["--", "cat"])
        .env("XDG_CONFIG_HOME", isolated_config_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tuipo");

    // A multi-line block with a couple of misspellings. The interior
    // newlines make this paste-shaped (signal 1), so it routes through
    // the fast paste path rather than the per-byte typing path.
    let lines = [
        "the quick brown fox jumps over the lazy dog",
        "i beleive this paragrah has a few typos in it",
        "pasting should not hang the wrapped program at all",
        "and every single line must reach the child intact",
    ];
    let block = format!("{}\n", lines.join("\n"));

    let mut stdin = child.stdin.take().unwrap();
    let writer = thread::spawn(move || {
        // Let the pump start reading so the block arrives as one chunk.
        thread::sleep(Duration::from_millis(500));
        stdin.write_all(block.as_bytes()).unwrap();
        stdin.flush().unwrap();
        thread::sleep(Duration::from_millis(500));
        drop(stdin);
    });

    let output = child.wait_with_output().expect("wait_with_output");
    writer.join().unwrap();

    let raw = String::from_utf8_lossy(&output.stdout);
    for line in lines {
        assert!(
            raw.contains(line),
            "pasted line missing from child output: {line:?}\n--- stdout ---\n{raw:?}",
        );
    }
}

#[test]
fn many_small_writes_are_all_delivered() {
    let script = "for i in $(seq 1 50); do printf 'line-%02d\\n' \"$i\"; done";
    let assert = tuipo().args(["--", "sh", "-c", script]).assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    for i in 1..=50 {
        let needle = format!("line-{i:02}");
        assert!(stdout.contains(&needle), "missing {needle}: full stdout was {stdout:?}");
    }
}

/// An idle terminal never loads harper: the spell engine starts on the
/// first keystroke, not when tuipo does.
#[test]
fn engine_is_not_started_without_typing() {
    let log_path =
        std::env::temp_dir().join(format!("tuipo-noinput-test-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log_path);
    tuipo()
        .args(["--", "sh", "-c", "sleep 1"])
        .env("TUIPO_DEBUG_LOG", &log_path)
        .assert()
        .success();
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);
    assert!(log.contains("BOOT"), "debug log was not written:\n{log}");
    assert!(!log.contains("spawn pid="), "engine started with no input:\n{log}");
}

/// The memory win: harper lives in a `tuipo __engine` child that exits
/// once typing stops for `TUIPO_ENGINE_IDLE_SECS`. Typing again brings it
/// back, and the respawned engine's lints still get painted.
#[test]
fn idle_engine_exits_and_respawns_on_next_keystroke() {
    use std::process::{Command, Stdio};
    use std::thread;

    let log_path = std::env::temp_dir().join(format!("tuipo-idle-test-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log_path);

    let mut child = Command::new(env!("CARGO_BIN_EXE_tuipo"))
        .args(["--", "cat"])
        .env("TUIPO_DEBUG_LOG", &log_path)
        .env("TUIPO_PLAIN_UNDERLINE", "1")
        .env("TUIPO_ENGINE_IDLE_SECS", "1")
        .env("XDG_CONFIG_HOME", isolated_config_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tuipo");

    let mut stdin = child.stdin.take().unwrap();
    let log = log_path.clone();
    let writer = thread::spawn(move || {
        type_slowly(&mut stdin, "teh ");
        assert!(
            wait_for_log(&log, "ready pid=", 1, ENGINE_READY_TIMEOUT),
            "first engine never became ready"
        );
        assert!(
            wait_for_log(&log, "stop reason=idle", 1, Duration::from_secs(15)),
            "idle engine was not stopped"
        );
        type_slowly(&mut stdin, "paragprah ");
        assert!(
            wait_for_log(&log, "ready pid=", 2, ENGINE_READY_TIMEOUT),
            "engine did not come back after the idle stop"
        );
        thread::sleep(Duration::from_millis(700));
        drop(stdin);
    });

    let output = child.wait_with_output().expect("wait_with_output");
    let typed = writer.join();
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);
    let raw = String::from_utf8_lossy(&output.stdout);
    assert!(typed.is_ok(), "typing script failed\n--- log ---\n{log}");
    assert!(
        raw.contains("\x1b[4mparagprah\x1b[24m"),
        "respawned engine's lint was not painted\n--- stdout ---\n{raw:?}\n--- log ---\n{log}",
    );
}

/// A dead engine costs a moment of lints, never the session: tuipo
/// notices, starts a new one on the next keystroke, and keeps painting.
#[test]
fn killed_engine_is_replaced() {
    use std::process::{Command, Stdio};
    use std::thread;

    let log_path = std::env::temp_dir().join(format!("tuipo-kill-test-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log_path);

    let mut child = Command::new(env!("CARGO_BIN_EXE_tuipo"))
        .args(["--", "cat"])
        .env("TUIPO_DEBUG_LOG", &log_path)
        .env("TUIPO_PLAIN_UNDERLINE", "1")
        .env("XDG_CONFIG_HOME", isolated_config_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tuipo");

    let mut stdin = child.stdin.take().unwrap();
    let log = log_path.clone();
    let writer = thread::spawn(move || {
        type_slowly(&mut stdin, "teh ");
        assert!(
            wait_for_log(&log, "ready pid=", 1, ENGINE_READY_TIMEOUT),
            "engine never became ready"
        );
        let pid = last_engine_pid(&log).expect("engine pid in debug log");
        let killed = Command::new("kill").args(["-9", &pid.to_string()]).status();
        assert!(killed.is_ok_and(|s| s.success()), "could not kill engine {pid}");
        assert!(
            wait_for_log(&log, "exited unexpectedly", 1, Duration::from_secs(10)),
            "engine death went unnoticed"
        );
        type_slowly(&mut stdin, "paragprah ");
        assert!(
            wait_for_log(&log, "ready pid=", 2, ENGINE_READY_TIMEOUT),
            "no replacement engine"
        );
        thread::sleep(Duration::from_millis(700));
        drop(stdin);
    });

    let output = child.wait_with_output().expect("wait_with_output");
    let typed = writer.join();
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);
    let raw = String::from_utf8_lossy(&output.stdout);
    assert!(typed.is_ok(), "typing script failed\n--- log ---\n{log}");
    assert!(
        raw.contains("\x1b[4mparagprah\x1b[24m"),
        "replacement engine's lint was not painted\n--- stdout ---\n{raw:?}\n--- log ---\n{log}",
    );
}

/// `tuipo __engine` speaks the length-prefixed protocol on stdin/stdout:
/// a hello frame once harper is warm, one response per request, and a
/// clean exit when its stdin closes.
#[test]
fn engine_subcommand_speaks_the_wire_protocol() {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};

    let mut engine = Command::new(env!("CARGO_BIN_EXE_tuipo"))
        .arg("__engine")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn engine");
    let mut stdin = engine.stdin.take().unwrap();
    let mut stdout = engine.stdout.take().unwrap();

    let read_frame = |r: &mut dyn Read| -> Vec<u8> {
        let mut len = [0u8; 4];
        r.read_exact(&mut len).unwrap();
        let mut payload = vec![0u8; u32::from_le_bytes(len) as usize];
        r.read_exact(&mut payload).unwrap();
        payload
    };

    let hello = read_frame(&mut stdout);
    assert!(hello.starts_with(b"TUIPO-ENGINE"), "bad hello: {hello:?}");

    let mut request = 7u64.to_le_bytes().to_vec();
    request.extend_from_slice(b"fix teh bug");
    stdin.write_all(&(request.len() as u32).to_le_bytes()).unwrap();
    stdin.write_all(&request).unwrap();
    stdin.flush().unwrap();

    let response = read_frame(&mut stdout);
    assert_eq!(response[..8], 7u64.to_le_bytes(), "response id");
    let count = u32::from_le_bytes(response[8..12].try_into().unwrap());
    assert!(count >= 1, "no issues reported for `fix teh bug`");
    assert!(
        response.windows(3).any(|w| w == b"teh"),
        "response doesn't mention `teh`"
    );

    drop(stdin);
    let status = engine.wait().unwrap();
    assert!(status.success(), "engine exited with {status}");
}

/// `tuipo off` makes `tuipo -- <cmd>` run the command directly — no PTY in
/// between, so a new tab's shell starts plain — and `tuipo on` undoes it.
#[test]
fn off_switch_runs_commands_unwrapped() {
    let config = fresh_config_dir("offexec");
    let probe = ["--", "sh", "-c", "[ -t 0 ] && echo wrapped || echo direct; echo active=$TUIPO_ACTIVE"];
    let run = || {
        let out = tuipo()
            .args(probe)
            .env("XDG_CONFIG_HOME", &config)
            .assert()
            .success();
        String::from_utf8_lossy(&out.get_output().stdout).into_owned()
    };

    assert!(run().contains("wrapped"), "on by default");
    assert!(tuipo_switch(&config, "off").contains("tuipo is off"));
    assert!(tuipo_switch(&config, "off").contains("already off"));
    let off = run();
    assert!(off.contains("direct"), "switched off but still wrapped: {off:?}");
    // The hook's re-entrance guard must hold, or a shell's rc file would
    // exec tuipo again in a loop.
    assert!(off.contains("active=1"), "{off:?}");

    assert!(tuipo_switch(&config, "on").contains("tuipo is on"));
    assert!(run().contains("wrapped"), "switched on but not wrapped");
    let _ = std::fs::remove_dir_all(&config);
}

/// A running session notices `tuipo off` on its own: it clears what it
/// underlined, stops linting and stops its engine; `tuipo on` brings it
/// all back.
#[test]
fn off_switch_pauses_and_resumes_running_sessions() {
    use std::process::{Command, Stdio};
    use std::thread;

    let config = fresh_config_dir("offlive");
    let log_path = std::env::temp_dir().join(format!("tuipo-offlive-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log_path);

    let mut child = Command::new(env!("CARGO_BIN_EXE_tuipo"))
        .args(["--", "cat"])
        .env("TUIPO_DEBUG_LOG", &log_path)
        .env("TUIPO_PLAIN_UNDERLINE", "1")
        .env("XDG_CONFIG_HOME", &config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tuipo");

    let mut stdin = child.stdin.take().unwrap();
    let (log, cfg) = (log_path.clone(), config.clone());
    let writer = thread::spawn(move || {
        type_slowly(&mut stdin, "teh ");
        assert!(wait_for_log(&log, "ready pid=", 1, ENGINE_READY_TIMEOUT), "engine never ready");
        thread::sleep(Duration::from_millis(500));

        tuipo_switch(&cfg, "off");
        assert!(wait_for_log(&log, "SWITCH   off", 1, Duration::from_secs(5)), "off not noticed");
        assert!(wait_for_log(&log, "stop reason=off", 1, Duration::from_secs(5)), "engine kept running");
        type_slowly(&mut stdin, "wrold ");
        thread::sleep(Duration::from_millis(700));
        type_slowly(&mut stdin, "\n");

        tuipo_switch(&cfg, "on");
        assert!(wait_for_log(&log, "SWITCH   on", 1, Duration::from_secs(5)), "on not noticed");
        type_slowly(&mut stdin, "abuot ");
        assert!(wait_for_log(&log, "ready pid=", 2, ENGINE_READY_TIMEOUT), "engine not back");
        thread::sleep(Duration::from_millis(700));
        drop(stdin);
    });

    let output = child.wait_with_output().expect("wait_with_output");
    let typed = writer.join();
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_dir_all(&config);
    let raw = String::from_utf8_lossy(&output.stdout);
    let ctx = || format!("\n--- stdout ---\n{raw:?}\n--- log ---\n{log}");
    assert!(typed.is_ok(), "typing script failed{}", ctx());
    assert!(raw.contains("\x1b[4mteh\x1b[24m"), "nothing painted before off{}", ctx());
    assert!(raw.contains("\x1b[24mteh"), "underline not cleared on off{}", ctx());
    assert!(!raw.contains("\x1b[4mwrold"), "painted while off{}", ctx());
    assert!(raw.contains("\x1b[4mabuot\x1b[24m"), "not painting after on{}", ctx());
}
