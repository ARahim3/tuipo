//! `tuipo init` — writes a shell hook so every new terminal session
//! automatically wraps itself in tuipo.
//!
//! The hook respects the `TUIPO_ACTIVE` env var as a re-entrance guard,
//! and uses `exec` so the wrapper replaces the shell process rather than
//! adding a parent. After install, every new terminal calls
//! `exec tuipo -- $SHELL` once during shell init, then the user is
//! inside a tuipo-wrapped shell for the rest of the session.

use std::path::PathBuf;

const MARKER: &str = "# tuipo — Grammarly-style spell-check for terminal TUIs";

#[derive(Debug, PartialEq, Eq)]
struct ShellHook {
    rc_path: PathBuf,
    hook_line: String,
}

pub fn run_init(args: &[String]) -> i32 {
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let force = args.iter().any(|a| a == "--force");

    let shell = std::env::var("SHELL").unwrap_or_default();
    let home = match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h),
        None => {
            eprintln!("tuipo init: $HOME is not set");
            return 1;
        }
    };

    let exe_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("tuipo init: couldn't determine my own path: {e}");
            return 1;
        }
    };

    let Some(hook) = detect_shell(&shell, &home, &exe_path) else {
        eprintln!(
            "tuipo init: couldn't determine your shell from $SHELL={shell:?}\n\
             Supported: zsh, bash. Add the equivalent line to your shell's rc file manually."
        );
        return 1;
    };

    if dry_run {
        println!("Would add to {}:", hook.rc_path.display());
        println!("  {MARKER}");
        println!("  {}", hook.hook_line);
        return 0;
    }

    let existing = std::fs::read_to_string(&hook.rc_path).unwrap_or_default();
    if existing.contains(&hook.hook_line) && !force {
        println!(
            "tuipo init: already installed in {}.\n\
             Open a new terminal to activate, or pass --force to reinstall.",
            hook.rc_path.display(),
        );
        return 0;
    }

    // Strip any previous tuipo hook lines before re-installing — handles
    // upgrades cleanly without accumulating dead lines (e.g. old PATH-based
    // hook lingering after we switched to absolute-path).
    let cleaned = strip_old_hook(&existing);
    let needs_separator = !cleaned.is_empty() && !cleaned.ends_with('\n');
    let new_content = format!(
        "{cleaned}{sep}{MARKER}\n{}\n",
        hook.hook_line,
        sep = if needs_separator { "\n\n" } else { "\n" },
    );
    if let Err(e) = std::fs::write(&hook.rc_path, new_content) {
        eprintln!(
            "tuipo init: failed to write {}: {e}",
            hook.rc_path.display()
        );
        return 1;
    }
    println!(
        "tuipo init: installed in {}.\n\
         Open a new terminal (or run `exec $SHELL`) to activate.",
        hook.rc_path.display(),
    );
    0
}

/// Remove any prior tuipo hook lines and their MARKER comments from rc
/// content. Recognises both the current absolute-path hook and the older
/// `command -v tuipo` style.
fn strip_old_hook(content: &str) -> String {
    let mut out_lines: Vec<&str> = Vec::with_capacity(content.lines().count());
    let mut skip_next_if_marker = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == MARKER {
            // Drop the marker; also drop the following hook line.
            skip_next_if_marker = true;
            continue;
        }
        if skip_next_if_marker {
            skip_next_if_marker = false;
            // Drop the immediately-following hook line.
            if trimmed.contains("TUIPO_ACTIVE") && trimmed.contains("exec") {
                continue;
            }
            // Otherwise put it back — the marker was orphaned.
            out_lines.push(line);
            continue;
        }
        // Also catch tuipo hook lines that were added by older versions
        // without the marker (or via manual edits).
        if trimmed.starts_with("[[ -z \"$TUIPO_ACTIVE\" ]]") && trimmed.contains("tuipo") {
            continue;
        }
        out_lines.push(line);
    }
    // Trim trailing blank lines.
    while out_lines.last().map(|l| l.trim().is_empty()) == Some(true) {
        out_lines.pop();
    }
    out_lines.join("\n")
}

fn detect_shell(
    shell_env: &str,
    home: &std::path::Path,
    exe_path: &std::path::Path,
) -> Option<ShellHook> {
    if shell_env.ends_with("/zsh") || shell_env == "zsh" {
        Some(ShellHook {
            rc_path: home.join(".zshrc"),
            hook_line: zsh_bash_hook(exe_path),
        })
    } else if shell_env.ends_with("/bash") || shell_env == "bash" {
        // On macOS, interactive bash reads .bash_profile rather than .bashrc;
        // prefer it if it exists, fall back to .bashrc.
        let bash_profile = home.join(".bash_profile");
        let bashrc = home.join(".bashrc");
        let rc_path = if bash_profile.exists() {
            bash_profile
        } else {
            bashrc
        };
        Some(ShellHook {
            rc_path,
            hook_line: zsh_bash_hook(exe_path),
        })
    } else {
        None
    }
}

/// Embed the absolute path to *this* tuipo binary in the hook line.
/// Avoids depending on `tuipo` being in `$PATH` after install.
fn zsh_bash_hook(exe_path: &std::path::Path) -> String {
    let p = exe_path.display();
    format!(
        r#"[[ -z "$TUIPO_ACTIVE" ]] && [[ -x "{p}" ]] && exec "{p}" -- "$SHELL""#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fake_exe() -> std::path::PathBuf {
        std::path::PathBuf::from("/usr/local/bin/tuipo")
    }

    #[test]
    fn detects_zsh_from_path() {
        let h = detect_shell("/bin/zsh", Path::new("/home/u"), &fake_exe());
        assert!(h.is_some());
        let h = h.unwrap();
        assert!(h.rc_path.ends_with(".zshrc"));
        assert!(h.hook_line.contains("TUIPO_ACTIVE"));
        assert!(h.hook_line.contains("/usr/local/bin/tuipo"));
    }

    #[test]
    fn detects_bash_from_path() {
        let h = detect_shell("/bin/bash", Path::new("/tmp/nonexistent-home"), &fake_exe());
        assert!(h.is_some());
        let h = h.unwrap();
        // No .bash_profile in fake home → .bashrc
        assert!(h.rc_path.ends_with(".bashrc"));
    }

    #[test]
    fn unknown_shell_is_rejected() {
        let h = detect_shell("/bin/fish", Path::new("/home/u"), &fake_exe());
        assert!(h.is_none());
    }

    #[test]
    fn empty_shell_env_is_rejected() {
        let h = detect_shell("", Path::new("/home/u"), &fake_exe());
        assert!(h.is_none());
    }

    #[test]
    fn hook_uses_quoted_shell_var() {
        let hook = zsh_bash_hook(&fake_exe());
        assert!(hook.contains(r#""$SHELL""#));
    }

    #[test]
    fn hook_is_idempotent_via_guard() {
        let hook = zsh_bash_hook(&fake_exe());
        assert!(hook.contains(r#"-z "$TUIPO_ACTIVE""#));
    }

    #[test]
    fn hook_embeds_absolute_path() {
        // Critical: the hook should NOT rely on `command -v tuipo`. It
        // must use the absolute path so it works regardless of $PATH.
        let exe = std::path::PathBuf::from("/Users/me/Code/tuipo/target/release/tuipo");
        let hook = zsh_bash_hook(&exe);
        assert!(
            hook.contains("/Users/me/Code/tuipo/target/release/tuipo"),
            "hook should embed the binary's absolute path, got: {hook}",
        );
        assert!(
            !hook.contains("command -v"),
            "hook shouldn't depend on PATH-based lookup; got: {hook}",
        );
    }

    #[test]
    fn hook_includes_executable_check() {
        // If the binary is moved/deleted, the hook should silently skip
        // rather than try to exec a non-existent file.
        let hook = zsh_bash_hook(&fake_exe());
        assert!(hook.contains("-x"));
    }

    #[test]
    fn strip_removes_marker_and_hook_block() {
        let rc = "\
# my existing zshrc
alias ll='ls -l'

# tuipo — Grammarly-style spell-check for terminal TUIs
[[ -z \"$TUIPO_ACTIVE\" ]] && command -v tuipo >/dev/null && exec tuipo -- \"$SHELL\"

# more user config
export FOO=bar
";
        let cleaned = strip_old_hook(rc);
        assert!(!cleaned.contains("TUIPO_ACTIVE"));
        assert!(!cleaned.contains("spell-check for terminal"));
        assert!(cleaned.contains("alias ll"));
        assert!(cleaned.contains("export FOO=bar"));
    }

    #[test]
    fn strip_handles_orphan_hook_without_marker() {
        let rc = "\
alias ll='ls -l'
[[ -z \"$TUIPO_ACTIVE\" ]] && exec /old/path/tuipo -- \"$SHELL\"
export FOO=bar
";
        let cleaned = strip_old_hook(rc);
        assert!(!cleaned.contains("TUIPO_ACTIVE"));
        assert!(cleaned.contains("alias ll"));
        assert!(cleaned.contains("export FOO=bar"));
    }

    #[test]
    fn strip_leaves_unrelated_lines_alone() {
        let rc = "\
alias ll='ls -l'
export FOO=bar
";
        assert_eq!(strip_old_hook(rc), rc.trim_end());
    }
}
