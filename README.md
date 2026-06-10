<div align="center">

# tuipo

**Grammarly for your terminal.**

The red squiggly line from your editor — now for the prompts you type into Claude
Code, Codex, Aider, and any other terminal app.

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Platform: macOS | Linux](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey.svg)](#install)
[![crates.io](https://img.shields.io/crates/v/tuipo.svg)](https://crates.io/crates/tuipo)

<img src="https://raw.githubusercontent.com/ARahim3/tuipo/main/tuipo_demo.gif" alt="tuipo underlining typos in a Claude Code prompt" width="720">

<sub><i>tuipo catching typos in a prompt to Claude Code. It never touches the app it's wrapping — the underlines are drawn on top.</i></sub>

</div>

## Why I made this

I type a lot of prose into terminal apps — long prompts for Claude Code and other
AI agents, commit messages, SQL, quick notes. None of them spell-check, so my typos
just sail straight through into prompts and history. My editor has had the red
squiggly line for twenty years; my terminal never got it.

So I built it. You wrap a terminal program once, and you get that squiggly line
back — in Claude Code, Aider, Codex, vim, `psql`, your shell, whatever. It only
shows up when you actually misspell something, and it never changes how the app
you're running behaves.

And a typo costs more here than in a text editor. To a language model, a
correctly-spelled word usually maps to a clean, familiar token; a misspelled one
breaks into odd subword fragments it rarely saw arranged that way in training. So it
spends effort guessing what you meant — and sometimes guesses wrong and quietly runs
with it. One fat-fingered word in a prompt can nudge a coding agent down the wrong
path before it's written a line.

A few things I cared about while building it:

- **It stays invisible until you need it.** Nothing pops up while you type; the
  underline appears in the small pause *after* you stop, which is when your eyes are
  actually scanning back over the line.
- **It never gets in the app's way.** tuipo sits between your keystrokes and the
  program and draws on top of the screen — it never rewrites what the app prints.
  The wrapped app behaves exactly as if you'd run it directly.
- **It only nags about real words.** Code-shaped tokens (`snake_case`, `--flags`,
  `paths/like/this`, `CamelCase`, backtick spans, numbers) are skipped, so it
  underlines your sentences, not your identifiers.
- **It's quiet by default.** Out of the box you get spelling underlines and nothing
  else. Grammar hints, the suggestion picker, Tab-to-fix — all off until you turn
  them on.

## Install

```bash
# Homebrew (macOS & Linux)
brew install ARahim3/tuipo/tuipo

# or with cargo
cargo install tuipo

# or the shell installer (URL is in each release's notes)
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/ARahim3/tuipo/releases/latest/download/tuipo-installer.sh | sh
```

<details>
<summary>From source</summary>

```bash
git clone https://github.com/ARahim3/tuipo
cd tuipo
cargo build --release
# binary at ./target/release/tuipo
```
</details>

## Getting started

### Turn it on everywhere (recommended)

Install a shell hook once, and every new terminal session is wrapped automatically —
you never have to think about it again:

```bash
tuipo init            # adds the hook to ~/.zshrc or ~/.bashrc
tuipo init --dry-run  # just print the line it would add, don't write anything
```

Open a new terminal and you're covered everywhere. (The hook guards against
re-entry, so it won't wrap itself.)

### Or just for one app

If you'd rather not have it always-on and only want it inside a specific app — say,
just Claude Code — skip the hook and wrap that command directly:

```bash
tuipo -- claude
tuipo -- aider
tuipo -- psql mydb
tuipo -- bash
```

Anything after `--` is the command tuipo wraps, run exactly as you normally would.

## Settings

tuipo is intentionally minimal by default. Turn on more when you want it:

```bash
tuipo config           # interactive settings editor
tuipo config --print   # print current settings as TOML
tuipo config --path    # show where the config file lives
```

Settings live in `~/.config/tuipo/config.toml` (all keys optional):

| Key          | Default  | What it does                                                       |
|--------------|----------|--------------------------------------------------------------------|
| `paint`      | `true`   | The inline underline overlay. Turn off to make tuipo do nothing.   |
| `underline`  | `"auto"` | Underline style: `auto`, `plain`, or `fancy` (curly red).          |
| `tab_fix`    | `false`  | Tab fixes the most recent misspelling with the top suggestion.     |
| `picker`     | `false`  | Pick among suggestions inline (hover tooltip + Tab).               |
| `grammar`    | `false`  | A small, careful set of grammar hints on top of spelling (below).  |
| `status_row` | `false`  | A little issue-count line at the bottom.                           |
| `pause_ms`   | `150`    | How long you have to pause before underlines show up.              |
| `hover_ms`   | `250`    | Idle time before the picker tooltip appears.                       |
| `dict_path`  | —        | Path to your own word list (one word per line).                    |

Got project names or jargon that keep getting flagged? Drop them in
`~/.config/tuipo/dict.txt`, one per line.

Every setting also has an env-var override for one-off sessions (env wins over the
file) — e.g. `TUIPO_PAINT_OFF=1`, `TUIPO_TAB_FIX=1`, `TUIPO_GRAMMAR=1`. Run
`tuipo --help` for the full list.

## What about grammar?

Spelling is the default; grammar is opt-in and kept deliberately small. Terminal
prompts aren't essays — they're full of imperatives, lowercase starts, fragments,
and no end punctuation, so broad grammar checking misfires constantly. With
`grammar = true`, tuipo surfaces only a narrow, high-confidence slice (subject–verb
agreement, malapropisms, eggcorns, common usage slips) and leaves the noisy stuff
off. It's tuned for how people actually type in a terminal, not lifted from a
desktop grammar checker.

## How it works

tuipo opens a pseudo-terminal, runs your command inside it, and passes bytes both
ways. On the way in, it reconstructs what you've typed and runs it through the
open-source [Harper](https://github.com/Automattic/harper) checker. On the way out,
it watches the screen the app draws so it can place each underline exactly under the
flagged word — using only relative cursor moves, so it never messes with scrollback
or the app's layout. When you pause, it paints. If you ask it to fix something, it
"types" the correction into the app's input (backspaces + the replacement), so the
app reacts as if you'd typed it yourself. The app is never modified and never sees
anything unusual.

## A few things you might wonder

**Does it slow down or interfere with the app I'm wrapping?**
No. Passthrough is byte-for-byte and the app behaves exactly as if you launched it
directly. Painting only happens during idle pauses, when the app is idle too.

**Will it mess with tab-completion or my keybindings?**
No. Tab passes straight through to the app by default, so shell completion, vim
indent, and slash-command menus keep working. Fixing is opt-in, and even then it
stays out of the way while you're typing forward.

**Which terminals work?**
macOS and Linux. You get curly red underlines where the terminal supports them, with
an automatic fallback to a plain underline where it doesn't (e.g. Apple Terminal).

**How do I turn it off for a bit?**
`TUIPO_PAINT_OFF=1 tuipo -- <cmd>`, or set `paint = false`.

## Contributing

Issues and PRs welcome. `cargo build`, `cargo test`, and please keep
`cargo clippy --all-targets -- -D warnings` clean before opening a PR.

## Credits

Spell and grammar checking is powered by [Harper](https://github.com/Automattic/harper).
Built in Rust with [portable-pty](https://crates.io/crates/portable-pty),
[vte](https://crates.io/crates/vte), and [crossterm](https://crates.io/crates/crossterm).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), your choice.
Unless you say otherwise, anything you contribute is licensed the same way.

---

<sub>tuipo is an independent project and isn't affiliated with, endorsed by, or sponsored by Grammarly, Anthropic (Claude), or anyone else mentioned here. Those names are only used to describe what tuipo works with, or — for "Grammarly for your terminal" — to give a quick feel for what it does. It's not the same engine or feature set. All trademarks belong to their owners.</sub>
