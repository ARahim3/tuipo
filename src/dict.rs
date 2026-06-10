//! Custom-dictionary loading.
//!
//! Two sources, merged at startup:
//! 1. **Bundled default** (`assets/default_dict.txt`, embedded via
//!    `include_str!` at compile time). Common AI tools (`claude`,
//!    `ultrathink`, `anthropic`), CLI commands (`grep`, `awk`, `kubectl`,
//!    `tmux`), languages/frameworks (`rust`, `kotlin`, `react`, `postgres`,
//!    `nodejs`), web acronyms (`http`, `tls`, `oauth`) — the words people
//!    routinely type in prose that harper would otherwise flag.
//! 2. **Per-user** (`~/.config/tuipo/dict.txt` or
//!    `$XDG_CONFIG_HOME/tuipo/dict.txt`). User entries override / extend
//!    the bundled list.
//!
//! Each non-empty, non-comment line in either source is a word that tuipo
//! will *not* flag as misspelled, even if harper would. Implemented as a
//! post-filter on harper's output rather than feeding words into harper's
//! dictionary — simpler and has the same user-visible effect. Comparison
//! is case-insensitive against the literal word.

use std::collections::HashSet;
use std::path::PathBuf;

/// Bundled defaults. Compiled into the binary so a fresh install has a
/// sensible baseline without the user touching anything.
const DEFAULT_DICT_TEXT: &str = include_str!("../assets/default_dict.txt");

#[derive(Debug, Default, Clone)]
pub struct CustomDict {
    words: HashSet<String>,
}

impl CustomDict {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load the bundled defaults + the user's dict file at the default
    /// path. Missing user file → returns just the defaults. Use
    /// [`Self::from_text`] / [`Self::from_path`] in tests to bypass the
    /// bundled set when you want an isolated dictionary.
    pub fn from_default_path() -> Self {
        let mut dict = Self::from_text(DEFAULT_DICT_TEXT);
        if let Some(path) = default_path()
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            dict.extend_from_text(&text);
        }
        dict
    }

    #[allow(dead_code)] // used by tests; kept as a stable API for future loaders.
    pub fn from_path(path: &std::path::Path) -> std::io::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(Self::from_text(&content))
    }

    pub fn from_text(text: &str) -> Self {
        let mut dict = Self::empty();
        dict.extend_from_text(text);
        dict
    }

    /// Merge another text source into this dict. Used to layer the
    /// per-user dict on top of the bundled defaults.
    pub fn extend_from_text(&mut self, text: &str) {
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            self.words.insert(line.to_lowercase());
        }
    }

    pub fn contains(&self, word: &str) -> bool {
        if self.words.is_empty() {
            return false;
        }
        self.words.contains(&word.to_lowercase())
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.words.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

fn default_path() -> Option<PathBuf> {
    // Config override wins: lets a user point at a shared team dict
    // without symlinking.
    if let Some(p) = crate::config::get().dict_path.as_ref() {
        return Some(expand_tilde(p));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("tuipo").join("dict.txt"));
    }
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("tuipo")
            .join("dict.txt"),
    )
}

/// Minimal `~`-expansion for paths read from the config. Only expands a
/// leading `~/` — we don't try to resolve `~user`, since the only realistic
/// use is the user's own home.
fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_yields_empty_dict() {
        let d = CustomDict::from_text("");
        assert!(d.is_empty());
        assert!(!d.contains("anything"));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let d = CustomDict::from_text("# my dict\n\nabdurrahim\n\n# trailing comment\ntuipo\n");
        assert_eq!(d.len(), 2);
        assert!(d.contains("abdurrahim"));
        assert!(d.contains("tuipo"));
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let d = CustomDict::from_text("Abdurrahim\nTUIPO\n");
        assert!(d.contains("abdurrahim"));
        assert!(d.contains("ABDURRAHIM"));
        assert!(d.contains("tuipo"));
        assert!(!d.contains("notinthere"));
    }

    #[test]
    fn whitespace_around_words_is_trimmed() {
        let d = CustomDict::from_text("  spacey  \n\ttabby\t\n");
        assert!(d.contains("spacey"));
        assert!(d.contains("tabby"));
    }

    #[test]
    fn empty_dict_contains_returns_false_fast() {
        let d = CustomDict::empty();
        assert!(!d.contains("anything"));
    }

    #[test]
    fn extend_from_text_merges_words() {
        let mut d = CustomDict::from_text("apple\nbanana\n");
        d.extend_from_text("cherry\n# comment\napple\n");
        // apple deduplicates; cherry adds; comment ignored.
        assert_eq!(d.len(), 3);
        for w in ["apple", "banana", "cherry"] {
            assert!(d.contains(w), "missing `{w}`");
        }
    }

    #[test]
    fn bundled_defaults_include_signature_words() {
        // Make the bundled list explicit-tested: the words the user
        // specifically asked for must be in there. Failure here means
        // someone edited assets/default_dict.txt without updating tests.
        let d = CustomDict::from_text(super::DEFAULT_DICT_TEXT);
        for w in [
            "claude",
            "ultrathink",
            "anthropic",
            "tuipo",
            "kubectl",
            "grep",
            "tmux",
            "ssh",
            "oauth",
            "postgres",
            "nodejs",
            "pdf",
            "pdfs",
        ] {
            assert!(d.contains(w), "default dict missing `{w}`");
        }
        assert!(d.len() > 100, "default dict is suspiciously small");
    }
}
