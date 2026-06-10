//! tuipo — Grammarly-style spell-check overlay for any TUI.

use clap::Parser;

mod buffer;
mod config;
mod config_tui;
mod debug;
mod dict;
mod echo;
mod event;
mod fix;
mod init;
mod input;
mod paint;
mod picker;
mod pty;
mod screen;
mod spell;
mod status;
mod term;

#[derive(Parser, Debug)]
#[command(version, about = "Spell-check overlay for any TUI", long_about = None)]
struct Args {
    /// Command and arguments to wrap. Use `--` to separate tuipo's flags
    /// from the wrapped command's flags, e.g. `tuipo -- claude --model opus`.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

fn main() {
    term::install_panic_hook();
    // Handle subcommands before clap parses, because clap's
    // `trailing_var_arg` would otherwise consume `init` as part of the
    // wrapped command. We intercept the first positional arg.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("init") {
        std::process::exit(init::run_init(&argv[1..]));
    }
    if argv.first().map(String::as_str) == Some("config") {
        // Config is loaded inside the TUI from the file directly, so we
        // intentionally do NOT call `config::load_global()` for this
        // subcommand — the user is editing the file, not running tuipo
        // against it.
        std::process::exit(config_tui::run(&argv[1..]));
    }
    if argv.first().map(String::as_str) == Some("help")
        || argv.first().map(String::as_str) == Some("--help")
        || argv.first().map(String::as_str) == Some("-h")
    {
        print_help();
        std::process::exit(0);
    }
    let args = Args::parse();
    // Load the persistent config once, before any thread starts. Env vars
    // still override individual settings — see `config::Config` for the
    // precedence rules.
    config::load_global();
    match pty::run(args.command) {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("tuipo: {err:#}");
            std::process::exit(1);
        }
    }
}

fn print_help() {
    println!("tuipo — Grammarly-style spell-check overlay for any TUI\n");
    println!("USAGE:");
    println!("  tuipo -- <command> [args...]    Wrap a command under tuipo");
    println!("  tuipo init [--dry-run]          Install shell hook for auto-wrap");
    println!("  tuipo config                    Interactive TUI to edit settings");
    println!("  tuipo config --print            Print current config as TOML");
    println!("  tuipo config --path             Print the config file path");
    println!("  tuipo --help                    Show this help");
    println!("  tuipo --version                 Show version");
    println!();
    println!("CONFIG:");
    println!("  ~/.config/tuipo/config.toml     Persistent settings (paint, underline,");
    println!("                                   tab_fix, picker, status_row, pause_ms,");
    println!("                                   hover_ms, dict_path). All optional.");
    println!("  ~/.config/tuipo/dict.txt        Per-user custom dictionary (one word/line).");
    println!();
    println!("ENV VARS (always override the config):");
    println!("  TUIPO_ACTIVE                    Set by tuipo on the wrapped child");
    println!("  TUIPO_DEBUG_LOG=<path>          Append-mode debug log file");
    println!("  TUIPO_PAINT_OFF=1               Disable all painting (passthrough only)");
    println!("  TUIPO_STATUS=on                 Show always-on issue count status row");
    println!("  TUIPO_TAB_FIX=1                 Opt in to Tab=apply-top-suggestion");
    println!("  TUIPO_GRAMMAR=1                 Opt in to narrow grammar lints");
    println!("  TUIPO_PLAIN_UNDERLINE=1         Force plain underline (Apple Terminal-safe)");
    println!("  TUIPO_FANCY_UNDERLINE=1         Force curly red underline (overrides Auto)");
}
