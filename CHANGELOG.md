# Changelog

## 0.2.0

### Much lighter on memory

- The spell checker no longer stays loaded in every terminal. It now runs in a small
  helper process (`tuipo __engine`) that starts when you start typing and exits
  about a minute after you stop, so an idle terminal uses about 2 MB instead of
  130–200 MB. Tune the delay with `TUIPO_ENGINE_IDLE_SECS`.
- Spell checking no longer runs before each keystroke is passed to the app, so it
  never holds up your typing.

### Catches more, with fewer false alarms

- New and on by default: doubled words ("the the"), the wrong article ("an new",
  "a hour"), and "could of" / "should of".
- With `grammar = true`: mixed-up their/they're, then/than, its/it's, lets/let's.
- Far fewer underlines on shell commands and technical text. Paths, flags and
  dotfiles (`src/lib.rs`, `-rf`, `--oneline`, `~/.zshrc`) are left alone, and so are
  common terminal and chat words (`stdin`, `args`, `jq`, `ok`, `pls`, `lgtm`),
  hyphenated words (`re-run`) and possessives.
- Better first suggestions for common typos ("abuot" → about, "waht" → what,
  "alot" → a lot) — the fix Tab applies when Tab-fix is on.
- A misspelled word that wraps onto the next line is now underlined, and phrase
  underlines skip the space between words.

### An off switch

- `tuipo off` switches tuipo off everywhere: open tabs stop underlining within a
  second, and new tabs start without it. `tuipo on` turns it back on.

### Upgrading

Nothing to do — the hook `tuipo init` installed keeps working, `tuipo off`
included. Terminals opened before the upgrade keep the old version until you open
new ones.
