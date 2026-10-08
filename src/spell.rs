//! Spell/grammar engine. Wraps `harper-core` and:
//!   - Converts harper's char-offset spans to byte offsets (matching our
//!     buffer's cursor model).
//!   - Filters out lints on code-shaped tokens (paths, snake_case, --flags,
//!     CamelCase, ALLCAPS, URLs) so we don't scream at every `useState`.
//!   - Collapses harper's 20-variant `LintKind` into a coarser
//!     [`IssueCategory`] for UI use, with per-rule overrides for the few
//!     rules surfaced regardless of their kind.
//!   - Second-guesses lints harper gets wrong on terminal text
//!     (`SpellChecker::keep`).

use std::sync::Arc;

use harper_core::Dialect;
use harper_core::Document;
use harper_core::linting::{Lint, LintGroup, LintKind, Suggestion};
use harper_core::parsers::PlainEnglish;
use harper_core::spell::{Dictionary, FstDictionary};

use crate::dict::{CustomDict, strip_possessive};

/// Rules surfaced with spelling — always on — whatever kind harper gives
/// them: mechanical slips with no false positives in our survey of
/// terminal prompts, shell commands and README/CLAUDE.md prose (Oct 2026).
/// `ToTwoToo` is listed because harper ≥ 2.1 re-kinds it from `Typo` to
/// `WordChoice`, which would otherwise hide it after an upgrade.
const ALWAYS_RULES: &[&str] = &["RepeatedWords", "AnA", "ModalOf", "ToTwoToo"];

/// Rules surfaced with `grammar = true` on top of the kind whitelist:
/// classic confusables whose kinds (raw `Grammar`, `Punctuation`,
/// `WordChoice`, `Miscellaneous`) are too noisy to enable wholesale.
const GRAMMAR_RULES: &[&str] = &["TheirToTheyre", "ThenThan", "ItsContraction", "LetsConfusion"];

/// Rules that judge a word by the one after it. While that next word is
/// still being typed they misfire ("a u" wants "an" until "unique" is
/// done), so they wait for it — see [`next_word_is_complete`].
const LOOKAHEAD_RULES: &[&str] = &["AnA", "TheirToTheyre", "ItsContraction", "LetsConfusion"];

/// Doubled words that are grammatical ("had had", "that that") or
/// deliberate ("very very", "bye bye", "ha ha"): not flagged as repeats.
const DELIBERATE_REPEATS: &[&str] = &["had", "that", "very", "so", "no", "bye", "ha"];

/// Missing-space slips that are almost always meant as two words, though
/// a one-letter edit also makes a word ("alot" → "allot", "alto"). When
/// harper's SplitWords offers one of these, it goes first.
const MISSING_SPACE_FIXES: &[&str] = &[
    "a lot", "a bit", "a little", "in fact", "each other", "every time", "at least",
    "as well", "no one", "of course", "thank you", "in case", "even though",
];

/// Cap on suggestions per spelling lint after merging and ranking.
const MAX_SUGGESTIONS: usize = 4;

/// Prefixes harper's dictionary doesn't list on their own but that
/// hyphenate onto real words ("re-run", "multi-line", "mis-parses").
const HYPHEN_PREFIXES: &[&str] = &[
    "re", "pre", "non", "multi", "mis", "co", "un", "sub", "anti", "inter", "semi", "post",
    "de", "dis", "ex", "bi", "tri", "mid", "mini", "micro", "macro", "self", "auto", "cross",
    "counter", "meta", "pseudo", "quasi", "ultra", "hyper", "super",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IssueCategory {
    Spelling,
    Grammar,
    Style,
    Other,
}

impl IssueCategory {
    /// Category for a lint from harper rule `rule`: the per-rule lists
    /// ([`ALWAYS_RULES`], [`GRAMMAR_RULES`]) win; otherwise the kind
    /// decides ([`Self::from_kind`]). `Spelling` therefore means "always
    /// on": misspellings, typos, and the mechanical slips in
    /// `ALWAYS_RULES`.
    fn from_rule(rule: &str, kind: LintKind) -> Self {
        if ALWAYS_RULES.contains(&rule) {
            Self::Spelling
        } else if GRAMMAR_RULES.contains(&rule) {
            Self::Grammar
        } else {
            Self::from_kind(kind)
        }
    }

    /// Map harper's fine-grained `LintKind` into our four-bucket
    /// taxonomy. Two key restrictions worth calling out:
    ///
    /// - **Only `Spelling`/`Typo` map to `Spelling`.** `BoundaryError`
    ///   was tried here once and produced false underlines on common
    ///   words (see pivot #8).
    /// - **`Grammar` is the narrow whitelist only.** Just the
    ///   high-precision kinds — subject-verb `Agreement`, classic
    ///   `Malapropism`s, `Eggcorn` substitutions, `Nonstandard`
    ///   fixed-phrase idioms (e.g. "for all intents and purposes"),
    ///   and `Usage` pedantry. The broader categories that harper also classifies
    ///   as grammar — raw `LintKind::Grammar` (which fires on
    ///   imperatives), `Punctuation` (terminal prompts have none),
    ///   `Capitalization` (prompts often start lowercase), and
    ///   `BoundaryError` — fall through to `Other` and are never
    ///   surfaced. The `grammar = true` config flag gates everything
    ///   that does map to `Grammar` here; users never see the
    ///   non-whitelisted kinds regardless of their config.
    fn from_kind(kind: LintKind) -> Self {
        match kind {
            LintKind::Spelling | LintKind::Typo => Self::Spelling,
            LintKind::Agreement
            | LintKind::Malapropism
            | LintKind::Eggcorn
            | LintKind::Nonstandard
            | LintKind::Usage => Self::Grammar,
            LintKind::Style
            | LintKind::WordChoice
            | LintKind::Enhancement
            | LintKind::Readability
            | LintKind::Redundancy
            | LintKind::Repetition => Self::Style,
            _ => Self::Other,
        }
    }
}

/// Whether the painter / picker should surface a lint of the given
/// category. Always true for `Spelling`; `Grammar` follows the user's
/// `grammar` config flag; nothing else is paintable today (`Style` is a
/// future opt-in; `Other` is harper's everything-else bucket and stays
/// hidden). Centralised here so consumers can share one predicate and
/// future category additions land in one place.
pub fn is_actionable_category(category: IssueCategory, grammar_enabled: bool) -> bool {
    match category {
        IssueCategory::Spelling => true,
        IssueCategory::Grammar => grammar_enabled,
        IssueCategory::Style | IssueCategory::Other => false,
    }
}

/// What we surface to the rest of the program. We carry *both* byte offsets
/// (for buffer/cursor work) and char offsets (for echo-tracker lookups,
/// which are char-indexed) so callers never have to convert between them.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SpellIssue {
    pub byte_start: usize,
    pub byte_end: usize,
    pub char_start: usize,
    pub char_end: usize,
    pub word: String,
    pub message: String,
    pub suggestions: Vec<String>,
    pub category: IssueCategory,
    /// Lower = more important (harper's convention; passed through verbatim).
    pub priority: u8,
    /// The harper rule that produced the lint (`SpellCheck`,
    /// `RepeatedWords`, …). Drives the per-rule category overrides and
    /// shows up in the debug log.
    pub rule: String,
}

pub struct SpellChecker {
    linter: LintGroup,
    /// harper's dictionary, for second opinions on its own lints
    /// (compounds, possessives, the word after an article).
    dictionary: Arc<FstDictionary>,
    custom: CustomDict,
}

impl SpellChecker {
    pub fn new() -> Self {
        Self::with_custom(CustomDict::from_default_path())
    }

    pub fn with_custom(custom: CustomDict) -> Self {
        let dictionary = FstDictionary::curated();
        let linter = LintGroup::new_curated(dictionary.clone(), Dialect::American);
        Self {
            linter,
            dictionary,
            custom,
        }
    }

    pub fn check(&mut self, text: &str) -> Vec<SpellIssue> {
        if text.trim().is_empty() {
            return Vec::new();
        }
        let doc = Document::new_curated(text, &PlainEnglish);
        // `organized_lints` is what `lint()` flattens; keeping the map
        // tells us which rule produced each lint.
        let organized = self.linter.organized_lints(&doc);

        // Build a char-offset → byte-offset prefix sum once, so each lint's
        // span conversion is O(1) instead of O(n).
        let mut char_to_byte: Vec<usize> = Vec::with_capacity(text.len() + 1);
        for (b, _) in text.char_indices() {
            char_to_byte.push(b);
        }
        char_to_byte.push(text.len()); // sentinel for end-of-string

        // Char view of the text, for extracting span content.
        let chars: Vec<char> = text.chars().collect();

        let mut issues = Vec::new();
        for (rule, lints) in organized {
            for lint in lints {
                if let Some(issue) = self.to_issue(&rule, lint, text, &chars, &char_to_byte) {
                    issues.push(issue);
                }
            }
        }
        self.consolidate_spelling(issues)
    }

    /// One spelling lint per span: rules that flag the same word
    /// (SpellCheck, SplitWords, …) are merged into the first, and a
    /// misspelling's suggestions are re-ranked (`rank_suggestions`).
    fn consolidate_spelling(&self, issues: Vec<SpellIssue>) -> Vec<SpellIssue> {
        let mut out: Vec<SpellIssue> = Vec::with_capacity(issues.len());
        for issue in issues {
            let same_span = out.iter_mut().find(|o| {
                o.category == IssueCategory::Spelling
                    && issue.category == IssueCategory::Spelling
                    && (o.char_start, o.char_end) == (issue.char_start, issue.char_end)
            });
            match same_span {
                Some(first) => first.suggestions.extend(issue.suggestions),
                None => out.push(issue),
            }
        }
        for issue in &mut out {
            if issue.rule == "SpellCheck" {
                let merged = std::mem::take(&mut issue.suggestions);
                issue.suggestions = self.rank_suggestions(&issue.word, merged);
            }
        }
        out
    }

    /// Order fixes for a misspelled `word`: a well-known missing-space fix
    /// ([`MISSING_SPACE_FIXES`]), then dictionary words one keyboard slip
    /// away ([`Self::keyboard_slips`]), then harper's own ranking. harper
    /// ranks by edit distance and frequency, which ties a slip with
    /// unrelated one-letter changes ("abuot" → "abbot" before "about").
    /// When its top pick restores a dropped letter ("ned" → "need") it is
    /// itself a slip, so it keeps its place. harper's SplitWords splits
    /// are mostly noise ("taht" → "ta ht"), so other splits only survive
    /// when there's nothing else to offer.
    fn rank_suggestions(&self, word: &str, suggestions: Vec<String>) -> Vec<String> {
        let is_known_fix = |s: &String| MISSING_SPACE_FIXES.contains(&s.to_lowercase().as_str());
        let has_single_word = suggestions.iter().any(|s| !s.contains(' '));
        let mut ranked: Vec<String> = suggestions.iter().filter(|s| is_known_fix(s)).cloned().collect();
        if !suggestions.first().is_some_and(|top| restores_dropped_letter(word, top)) {
            ranked.extend(self.keyboard_slips(word));
        }
        ranked.extend(
            suggestions
                .into_iter()
                .filter(|s| !s.contains(' ') || !has_single_word),
        );
        let mut seen = std::collections::HashSet::new();
        ranked.retain(|s| seen.insert(s.to_lowercase()));
        ranked.truncate(MAX_SUGGESTIONS);
        ranked
    }

    /// Dictionary words one keyboard slip from `word`: two adjacent letters
    /// swapped ("teh", "waht") or a letter typed twice ("comming"). Exact
    /// lowercase lookups, so proper nouns don't creep in ("adn" → "and",
    /// not "Dan"); a capitalized typo gets capitalized fixes.
    fn keyboard_slips(&self, word: &str) -> Vec<String> {
        let chars: Vec<char> = word.chars().collect();
        if chars.len() < 2 || !chars.iter().all(char::is_ascii_alphabetic) {
            return Vec::new();
        }
        let capitalized = chars[0].is_ascii_uppercase() && chars[1..].iter().all(char::is_ascii_lowercase);
        if !capitalized && !chars.iter().all(char::is_ascii_lowercase) {
            return Vec::new();
        }
        let lower: Vec<char> = chars.iter().map(char::to_ascii_lowercase).collect();
        let mut slips = Vec::new();
        for i in 0..lower.len() - 1 {
            let mut candidate = lower.clone();
            if lower[i] == lower[i + 1] {
                candidate.remove(i);
            } else {
                candidate.swap(i, i + 1);
            }
            let candidate: String = candidate.into_iter().collect();
            if self.dictionary.contains_exact_word_str(&candidate) {
                slips.push(if capitalized { capitalize(&candidate) } else { candidate });
            }
        }
        slips
    }

    fn to_issue(
        &self,
        rule: &str,
        lint: Lint,
        text: &str,
        chars: &[char],
        char_to_byte: &[usize],
    ) -> Option<SpellIssue> {
        let (char_start, char_end) = (lint.span.start, lint.span.end);
        let byte_start = *char_to_byte.get(char_start)?;
        let byte_end = *char_to_byte.get(char_end)?;
        let word: String = chars.get(char_start..char_end)?.iter().collect();

        if looks_like_code(&word, text, byte_start, byte_end) || self.custom.contains(&word) {
            return None;
        }
        let category = IssueCategory::from_rule(rule, lint.lint_kind);
        if !self.keep(rule, category, &word, text, byte_start, byte_end) {
            return None;
        }

        let suggestions = lint
            .suggestions
            .iter()
            .filter_map(suggestion_replacement)
            .collect();

        Some(SpellIssue {
            byte_start,
            byte_end,
            char_start,
            char_end,
            word,
            message: lint.message,
            suggestions,
            category,
            priority: lint.priority,
            rule: rule.to_string(),
        })
    }

    /// Second opinion on a lint harper produced, for cases where it gets
    /// terminal text wrong. `false` drops the lint.
    fn keep(
        &self,
        rule: &str,
        category: IssueCategory,
        word: &str,
        text: &str,
        byte_start: usize,
        byte_end: usize,
    ) -> bool {
        if LOOKAHEAD_RULES.contains(&rule) && !next_word_is_complete(text, byte_end) {
            return false;
        }
        match rule {
            "RepeatedWords" => !is_deliberate_repeat(word),
            "AnA" => self.article_fix_is_reliable(text, byte_end),
            _ if category == IssueCategory::Spelling && !word.contains(char::is_whitespace) => {
                // A lone letter is a unit ("60 s"), a variable or a
                // fragment of a mangled token ("That;s"), not a word to
                // correct.
                word.chars().count() > 1
                    && !self.is_known_compound_or_possessive(text, byte_start, byte_end)
            }
            _ => true,
        }
    }

    /// a/an depends on how the next word sounds. harper knows that for
    /// dictionary words and spells out ALLCAPS initialisms ("an LLM"); for
    /// anything else it guesses from the first letter and is often wrong:
    /// lowercase initialisms it reads as words ("an npm package" → "a"),
    /// identifiers (`x86`, backticked code), words it doesn't know.
    fn article_fix_is_reliable(&self, text: &str, byte_end: usize) -> bool {
        let Some(next) = text[byte_end..].split_whitespace().next() else {
            return true;
        };
        let next = trim_prose_punct(next);
        if next.is_empty() || next.chars().all(|c| c.is_ascii_uppercase()) {
            return true;
        }
        // No vowels (`npm`, `ssh`, `rpm`): said letter by letter.
        let has_vowel = next.chars().any(|c| "aeiouyAEIOUY".contains(c));
        has_vowel && !looks_like_code_token(next) && self.dictionary.contains_word_str(next)
    }

    /// harper's dictionary lacks most hyphenated compounds ("re-run",
    /// "multi-line") and the possessives of words it knows ("harper's"),
    /// so its spell check flags them — or a fragment of them ("mis" in
    /// "mis-parses"). True when the whole token the lint sits in is one.
    fn is_known_compound_or_possessive(&self, text: &str, byte_start: usize, byte_end: usize) -> bool {
        let (start, end) = token_bounds(text, byte_start, byte_end);
        let token = trim_prose_punct(&text[start..end]);
        match strip_possessive(token) {
            Some(base) => self.is_known(base) || self.is_known_compound(base),
            None => self.is_known_compound(token),
        }
    }

    fn is_known_compound(&self, token: &str) -> bool {
        token.contains('-')
            && !token.starts_with('-')
            && token.split('-').all(|part| {
                !part.is_empty()
                    && (self.is_known(part)
                        || HYPHEN_PREFIXES.contains(&part.to_lowercase().as_str()))
            })
    }

    fn is_known(&self, word: &str) -> bool {
        self.dictionary.contains_word_str(word) || self.custom.contains(word)
    }
}

/// Whether `fix` is `typo` with one letter put back ("ned" → "need").
fn restores_dropped_letter(typo: &str, fix: &str) -> bool {
    let typo: Vec<char> = typo.chars().flat_map(char::to_lowercase).collect();
    let fix: Vec<char> = fix.chars().flat_map(char::to_lowercase).collect();
    fix.len() == typo.len() + 1
        && (0..fix.len()).any(|skip| {
            fix.iter()
                .enumerate()
                .filter(|&(i, _)| i != skip)
                .map(|(_, c)| c)
                .eq(typo.iter())
        })
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// "had had", "very very", …: see [`DELIBERATE_REPEATS`].
fn is_deliberate_repeat(span: &str) -> bool {
    span.split_whitespace()
        .next()
        .is_some_and(|w| DELIBERATE_REPEATS.contains(&w.to_lowercase().as_str()))
}

/// Whether the word after `byte_end` is finished: followed by whitespace,
/// or ending in punctuation the user typed after it. No next word at all
/// counts as finished — the painter's own rule already holds back a lint
/// on the word being typed.
fn next_word_is_complete(text: &str, byte_end: usize) -> bool {
    let rest = text[byte_end..].trim_start();
    let Some(word) = rest.split_whitespace().next() else {
        return true;
    };
    word.len() < rest.len() || word.chars().next_back().is_some_and(|c| !c.is_alphanumeric())
}

/// Byte range of the whitespace-delimited token(s) covering
/// `byte_start..byte_end`.
fn token_bounds(text: &str, byte_start: usize, byte_end: usize) -> (usize, usize) {
    let start = text[..byte_start]
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let end = text[byte_end..]
        .find(char::is_whitespace)
        .map_or(text.len(), |i| byte_end + i);
    (start, end)
}

/// Strip the punctuation prose wraps around a word — quotes, parentheses,
/// markdown emphasis, trailing commas / periods / colons — but keep what
/// makes a token look like code: a leading `-` (flags), `.` (dotfiles),
/// `~` or `/` (paths), and braces / angle brackets (placeholders).
fn trim_prose_punct(token: &str) -> &str {
    token
        .trim_start_matches(['"', '\'', '(', '[', '*', '\u{201c}', '\u{2018}'])
        .trim_end_matches([
            '"', '\'', ')', ']', '*', ',', '.', ';', ':', '!', '?', '\u{201d}', '\u{2019}',
        ])
}

impl Default for SpellChecker {
    fn default() -> Self {
        Self::new()
    }
}

fn suggestion_replacement(s: &Suggestion) -> Option<String> {
    match s {
        Suggestion::ReplaceWith(chars) => Some(chars.iter().collect()),
        Suggestion::InsertAfter(chars) => Some(chars.iter().collect()),
        Suggestion::Remove => None,
    }
}

/// Heuristic: is this span shaped like code/identifier/path/flag rather
/// than natural-language prose? Better to err on the side of "yes, skip
/// it" — a missed lint is less annoying than flagging `useState` every
/// time.
///
/// Judged on the span's own words *and* on the whole whitespace-delimited
/// token(s) it touches: harper splits `src/lib.rs`, `-rf`, `--oneline`,
/// `~/.zshrc` and `src.bak` into fragments (`src`, `rs`, `rf`, …) that
/// look like prose on their own, while an acronym inside a hyphenated
/// token (`stale-SGR`) only looks like code on its own. If *anything*
/// checked is code-shaped the lint is skipped — we'd rather miss a
/// grammar warning around mixed code/prose than paint underlines under
/// identifiers.
fn looks_like_code(word: &str, full_text: &str, byte_start: usize, byte_end: usize) -> bool {
    if word.is_empty() {
        return true;
    }
    let (start, end) = token_bounds(full_text, byte_start, byte_end);
    word.split_whitespace()
        .chain(full_text[start..end].split_whitespace())
        .any(|token| looks_like_code_token(trim_prose_punct(token)))
}

/// Token-level shape check, on a token with its surrounding prose
/// punctuation already trimmed (see `looks_like_code`).
fn looks_like_code_token(word: &str) -> bool {
    // "PTY's" is shaped like "PTY".
    let word = strip_possessive(word).unwrap_or(word);
    if word.is_empty() {
        return true;
    }
    let chars: Vec<char> = word.chars().collect();

    // Contains structural punctuation/symbols typical of identifiers,
    // paths, URLs, assignments and placeholders.
    if word.contains([
        '/', '\\', '_', '@', '#', '$', ':', '`', '=', '{', '}', '<', '>',
    ]) {
        return true;
    }

    // Starts with '-' (flag) or '.' + a letter (dotfile, `.venv`).
    if word.starts_with('-')
        || (word.starts_with('.') && chars.get(1).is_some_and(|c| c.is_alphanumeric()))
    {
        return true;
    }

    // Digits adjacent to letters (versions, IDs, `30s`).
    let has_letter = chars.iter().any(|c| c.is_alphabetic());
    let has_digit = chars.iter().any(|c| c.is_ascii_digit());
    if has_letter && has_digit {
        return true;
    }

    // Dot within the word (not trailing): foo.bar, file.txt.
    if let Some(idx) = word.find('.')
        && idx > 0
        && idx + 1 < word.len()
    {
        return true;
    }

    // All-caps two chars or more — likely an acronym/constant — or its
    // plural (`PRs`, `CSIs`).
    let acronym = chars.strip_suffix(&['s']).unwrap_or(&chars);
    if acronym.len() >= 2 && acronym.iter().all(|c| c.is_ascii_uppercase()) {
        return true;
    }

    // CamelCase: lowercase letter directly followed by uppercase, anywhere.
    let mut prev: Option<char> = None;
    for &c in &chars {
        if let Some(p) = prev
            && p.is_ascii_lowercase()
            && c.is_ascii_uppercase()
        {
            return true;
        }
        prev = Some(c);
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(text: &str) -> Vec<SpellIssue> {
        SpellChecker::new().check(text)
    }

    #[test]
    fn empty_text_has_no_issues() {
        assert!(check("").is_empty());
        assert!(check("   ").is_empty());
    }

    #[test]
    fn clean_text_has_no_spelling_issues() {
        let issues = check("Hello world.");
        let spelling: Vec<_> = issues
            .iter()
            .filter(|i| i.category == IssueCategory::Spelling)
            .collect();
        assert!(spelling.is_empty(), "unexpected spelling issues: {spelling:?}");
    }

    #[test]
    fn obvious_misspelling_is_flagged_with_suggestion() {
        let issues = check("teh cat");
        let teh = issues
            .iter()
            .find(|i| i.word.eq_ignore_ascii_case("teh"))
            .expect("expected `teh` to be flagged");
        assert_eq!(teh.category, IssueCategory::Spelling);
        assert!(
            teh.suggestions.iter().any(|s| s == "the"),
            "expected `the` in suggestions, got {:?}",
            teh.suggestions
        );
    }

    #[test]
    fn byte_offsets_locate_misspelling_correctly() {
        let text = "hello teh cat";
        let issues = check(text);
        let teh = issues
            .iter()
            .find(|i| i.word.eq_ignore_ascii_case("teh"))
            .expect("teh not flagged");
        assert_eq!(&text[teh.byte_start..teh.byte_end], "teh");
    }

    #[test]
    fn byte_offsets_correct_with_leading_multibyte() {
        // 'café' is 5 bytes (4 chars). The misspelling 'teh' starts at byte 6.
        let text = "café teh cat";
        let issues = check(text);
        let teh = issues
            .iter()
            .find(|i| i.word.eq_ignore_ascii_case("teh"))
            .expect("teh not flagged");
        assert_eq!(&text[teh.byte_start..teh.byte_end], "teh");
    }

    #[test]
    fn byte_offsets_correct_with_emoji() {
        // '🦀' is 4 bytes (1 char).
        let text = "🦀 teh cat";
        let issues = check(text);
        let teh = issues
            .iter()
            .find(|i| i.word.eq_ignore_ascii_case("teh"))
            .expect("teh not flagged");
        assert_eq!(&text[teh.byte_start..teh.byte_end], "teh");
    }

    #[test]
    fn snake_case_is_skipped() {
        // `mispelled_variable` is misspelled in prose terms but is code.
        let issues = check("the mispelled_variable is here");
        assert!(
            !issues.iter().any(|i| i.word.contains('_')),
            "snake_case got flagged: {issues:?}",
        );
    }

    #[test]
    fn camel_case_is_skipped() {
        let issues = check("call useState here");
        assert!(
            !issues.iter().any(|i| i.word == "useState"),
            "camelCase got flagged: {issues:?}",
        );
    }

    #[test]
    fn all_caps_is_skipped() {
        let issues = check("set the API_KEY value");
        assert!(
            !issues
                .iter()
                .any(|i| i.word == "API" || i.word == "API_KEY"),
            "ALLCAPS got flagged: {issues:?}",
        );
    }

    #[test]
    fn paths_are_skipped() {
        let issues = check("open src/main.rs to edit");
        assert!(
            !issues.iter().any(|i| i.word.contains('/')),
            "path got flagged: {issues:?}",
        );
    }

    #[test]
    fn flags_are_skipped() {
        let issues = check("pass --max-tokens to the call");
        assert!(
            !issues.iter().any(|i| i.word.starts_with('-')),
            "flag got flagged: {issues:?}",
        );
    }

    #[test]
    fn version_like_tokens_are_skipped() {
        let issues = check("install rust 1.93 now");
        assert!(
            !issues.iter().any(|i| i.word.chars().any(|c| c.is_ascii_digit())),
            "version-like token got flagged: {issues:?}",
        );
    }

    #[test]
    fn backtick_wrapped_is_skipped() {
        let issues = check("the `mispelt` identifier");
        assert!(
            !issues.iter().any(|i| i.word == "mispelt"),
            "backtick-wrapped got flagged: {issues:?}",
        );
    }

    #[test]
    fn real_sentence_only_flags_actual_misspellings_as_spelling() {
        // Verbatim sentence from a user-reported visual bug ("every word
        // underlined"). Locks down the contract that the painter's
        // Spelling-category filter relies on: common correctly-spelled
        // words must not be in the Spelling bucket. If this test fails,
        // either harper's behavior changed or the IssueCategory mapping
        // is wrong — both warrant investigation before chasing the paint
        // layer.
        let text = "write the reason for the peple of US India";
        let issues = check(text);
        let all: Vec<String> = issues
            .iter()
            .map(|i| format!("{}:{:?}", i.word, i.category))
            .collect();
        let spelling_words: Vec<String> = issues
            .iter()
            .filter(|i| i.category == IssueCategory::Spelling)
            .map(|i| i.word.to_lowercase())
            .collect();
        let common = ["write", "the", "reason", "for", "of", "us", "india"];
        for w in common {
            assert!(
                !spelling_words.iter().any(|s| s == w),
                "common word `{w}` got flagged as Spelling. All lints: {all:?}",
            );
        }
    }

    #[test]
    fn multiple_misspellings_all_reported() {
        let issues = check("teh quikc brown fox jumpd over");
        let spelling_words: Vec<&str> = issues
            .iter()
            .filter(|i| i.category == IssueCategory::Spelling)
            .map(|i| i.word.as_str())
            .collect();
        // At least 'teh' and one of 'quikc'/'jumpd' must show up.
        assert!(
            spelling_words.contains(&"teh"),
            "expected teh in: {spelling_words:?}",
        );
    }

    #[test]
    fn looks_like_code_multi_token_skips_when_any_token_is_code() {
        // Multi-token span containing an ALL_CAPS acronym (`API`) should
        // be treated as code-adjacent and skipped. Same for spans
        // containing CamelCase or paths.
        assert!(looks_like_code("the API is", "the API is broken", 0, 10));
        assert!(looks_like_code("useState should", "useState should not", 0, 15));
        assert!(looks_like_code("src/main.rs is", "src/main.rs is open", 0, 14));
    }

    #[test]
    fn looks_like_code_multi_token_passes_clean_prose() {
        // No code-shaped token anywhere → don't skip. This is what makes
        // grammar lints reach the painter on real prose.
        assert!(!looks_like_code("the cat is", "the cat is here", 0, 10));
        assert!(!looks_like_code("there are two", "there are two reasons", 0, 13));
    }

    #[test]
    fn issue_category_from_kind_narrow_grammar_whitelist() {
        // Only the five high-precision kinds map to Grammar. Everything
        // else that harper used to classify under "grammar-flavored"
        // (raw Grammar, Punctuation, Capitalization, BoundaryError)
        // falls through to Other and is never surfaced, no matter what
        // the user's config says. Locking this down here so a future
        // refactor doesn't quietly re-enable the noisy kinds.
        assert_eq!(IssueCategory::from_kind(LintKind::Spelling), IssueCategory::Spelling);
        assert_eq!(IssueCategory::from_kind(LintKind::Typo), IssueCategory::Spelling);
        assert_eq!(IssueCategory::from_kind(LintKind::Agreement), IssueCategory::Grammar);
        assert_eq!(IssueCategory::from_kind(LintKind::Malapropism), IssueCategory::Grammar);
        assert_eq!(IssueCategory::from_kind(LintKind::Eggcorn), IssueCategory::Grammar);
        assert_eq!(IssueCategory::from_kind(LintKind::Nonstandard), IssueCategory::Grammar);
        assert_eq!(IssueCategory::from_kind(LintKind::Usage), IssueCategory::Grammar);
        assert_eq!(IssueCategory::from_kind(LintKind::Grammar), IssueCategory::Other);
        assert_eq!(IssueCategory::from_kind(LintKind::Punctuation), IssueCategory::Other);
        assert_eq!(IssueCategory::from_kind(LintKind::Capitalization), IssueCategory::Other);
        assert_eq!(IssueCategory::from_kind(LintKind::BoundaryError), IssueCategory::Other);
        assert_eq!(IssueCategory::from_kind(LintKind::Style), IssueCategory::Style);
        assert_eq!(IssueCategory::from_kind(LintKind::Repetition), IssueCategory::Style);
    }

    #[test]
    fn agreement_error_in_real_prose_lands_in_grammar_category() {
        // End-to-end check: feed harper a sentence with a verb-form
        // agreement error and verify that *some* lint surfaces in
        // IssueCategory::Grammar. The contract: when grammar checking is
        // on, real grammar errors aren't silently dropped.
        //
        // Sentence picked from the `harper_grammar_probe` diagnostic
        // below — harper reliably flags "he go to the store" as a
        // grammar issue on the verb "go". If harper's rules ever
        // change, run the probe to find a new robust sentence.
        let issues = check("he go to the store");
        let grammar_lints: Vec<&SpellIssue> = issues
            .iter()
            .filter(|i| i.category == IssueCategory::Grammar)
            .collect();
        assert!(
            !grammar_lints.is_empty(),
            "expected at least one Grammar-category lint; got: {:?}",
            issues
                .iter()
                .map(|i| (i.word.clone(), i.category))
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn nonstandard_idiom_lands_in_grammar_category() {
        // "for all intensive purposes" → "for all intents and purposes"
        // is a fixed-phrase idiom harper tags as LintKind::Nonstandard.
        // It must reach IssueCategory::Grammar so `grammar = true`
        // surfaces it (the broken-idiom slice of Grammarly-for-terminal).
        let issues = check("we need it for all intensive purposes here");
        let grammar_lints: Vec<&SpellIssue> = issues
            .iter()
            .filter(|i| i.category == IssueCategory::Grammar)
            .collect();
        assert!(
            !grammar_lints.is_empty(),
            "expected the idiom to land in Grammar; got: {:?}",
            issues
                .iter()
                .map(|i| (i.word.clone(), i.category))
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    #[ignore = "diagnostic probe — run with --ignored --nocapture"]
    fn harper_grammar_probe() {
        // Diagnostic probe — print every lint harper produces against a
        // small bank of sentences with classic agreement / malapropism /
        // eggcorn / usage errors. Run with `cargo test --bins
        // harper_grammar_probe -- --ignored --nocapture` if you need to
        // inspect what categories show up. Useful when picking robust
        // grammar sentences for tests, or when chasing a regression in
        // the grammar mapping.
        let sentences = [
            // Agreement
            "he go to the store",
            "she don't know that",
            "they was waiting",
            "the cats is running fast",
            // Malapropism / eggcorn
            "for all intensive purposes",
            "a mute point now",
            "the deep-seeded fear",
            "tow the line strictly",
            // Usage
            "between you and I",
            "i should of done it",
            "less people came",
            "fewer water is left",
            // Spelling (sanity)
            "irregardless of the reason",
            "teh quikc brown fox",
        ];
        for s in sentences {
            let issues = check(s);
            let summary: Vec<String> = issues
                .iter()
                .map(|i| format!("{}:{:?}", i.word, i.category))
                .collect();
            eprintln!("[probe] {s:?} -> {summary:?}");
        }
    }

    #[test]
    fn is_actionable_category_predicate() {
        // Spelling is always paintable regardless of the grammar flag.
        assert!(is_actionable_category(IssueCategory::Spelling, false));
        assert!(is_actionable_category(IssueCategory::Spelling, true));
        // Grammar follows the flag.
        assert!(!is_actionable_category(IssueCategory::Grammar, false));
        assert!(is_actionable_category(IssueCategory::Grammar, true));
        // Style and Other are never paintable today.
        assert!(!is_actionable_category(IssueCategory::Style, true));
        assert!(!is_actionable_category(IssueCategory::Other, true));
    }

    /// No custom dictionary: tests of tuipo's own filtering mustn't pass
    /// because the bundled dict happens to list a word.
    fn check_raw(text: &str) -> Vec<SpellIssue> {
        SpellChecker::with_custom(CustomDict::empty()).check(text)
    }

    fn by_rule<'a>(issues: &'a [SpellIssue], rule: &str) -> Option<&'a SpellIssue> {
        issues.iter().find(|i| i.rule == rule)
    }

    /// First suggestion of the spelling lint on `word` (other categories,
    /// like a hidden "ned" → "Ned" capitalization lint, don't count).
    fn top_fix(issues: &[SpellIssue], word: &str) -> Option<String> {
        let issue = issues
            .iter()
            .find(|i| i.word == word && i.category == IssueCategory::Spelling)?;
        issue.suggestions.first().cloned()
    }

    #[test]
    fn repeated_word_is_an_always_on_lint() {
        let issues = check_raw("fix the the bug now");
        let rep = by_rule(&issues, "RepeatedWords").expect("`the the` flagged");
        assert_eq!(rep.word, "the the");
        assert_eq!(rep.category, IssueCategory::Spelling);
        assert_eq!(rep.suggestions, vec!["the"]);
    }

    #[test]
    fn deliberate_repeats_are_left_alone() {
        for text in [
            "she had had enough today",
            "I think that that is fine",
            "it is very very slow",
            "bye bye old parser",
            "ha ha nice one",
        ] {
            let issues = check_raw(text);
            assert!(by_rule(&issues, "RepeatedWords").is_none(), "{text:?}: {issues:?}");
        }
    }

    #[test]
    fn article_mismatch_is_an_always_on_lint() {
        let issues = check_raw("add an new endpoint now");
        let ana = by_rule(&issues, "AnA").expect("`an new` flagged");
        assert_eq!((ana.word.as_str(), ana.category), ("an", IssueCategory::Spelling));
        assert_eq!(ana.suggestions, vec!["a"]);
        let issues = check_raw("this is a important change");
        assert_eq!(by_rule(&issues, "AnA").map(|i| i.suggestions.clone()), Some(vec!["an".into()]));
    }

    #[test]
    fn article_check_waits_for_the_next_word() {
        // Mid-word, "a importa…" can't be judged yet; once the word is
        // finished — by a space or punctuation — it can.
        assert!(by_rule(&check_raw("this is a importa"), "AnA").is_none());
        assert!(by_rule(&check_raw("this is a important change"), "AnA").is_some());
        assert!(by_rule(&check_raw("this is a important."), "AnA").is_some());
    }

    #[test]
    fn article_check_skips_words_harper_cannot_pronounce() {
        for text in [
            "publish an npm package today",
            "it runs on an x86 machine",
            "plus an `(row, col)` table here",
        ] {
            assert!(by_rule(&check_raw(text), "AnA").is_none(), "{text:?}");
        }
        // ALLCAPS initialisms are spelled out, so harper gets these right.
        assert!(by_rule(&check_raw("ask a LLM to help"), "AnA").is_some());
    }

    #[test]
    fn could_of_is_an_always_on_lint() {
        let issues = check_raw("it could of been worse");
        let modal = by_rule(&issues, "ModalOf").expect("`could of` flagged");
        assert_eq!(modal.category, IssueCategory::Spelling);
        assert_eq!(modal.suggestions.first().map(String::as_str), Some("could have"));
    }

    #[test]
    fn confusables_join_the_grammar_slice() {
        for (text, rule) in [
            ("their going to merge it tomorrow", "TheirToTheyre"),
            ("the new build is faster then before", "ThenThan"),
            ("its broken again after the merge", "ItsContraction"),
            ("lets fix the parser first", "LetsConfusion"),
        ] {
            let issues = check_raw(text);
            let lint = by_rule(&issues, rule)
                .unwrap_or_else(|| panic!("{rule} missing for {text:?}: {issues:?}"));
            assert_eq!(lint.category, IssueCategory::Grammar, "{text:?}");
        }
    }

    #[test]
    fn issue_category_rule_overrides() {
        use IssueCategory::*;
        assert_eq!(IssueCategory::from_rule("RepeatedWords", LintKind::Repetition), Spelling);
        assert_eq!(IssueCategory::from_rule("AnA", LintKind::Miscellaneous), Spelling);
        assert_eq!(IssueCategory::from_rule("ModalOf", LintKind::WordChoice), Spelling);
        // harper ≥ 2.1 re-kinds to/too as WordChoice; it must stay visible.
        assert_eq!(IssueCategory::from_rule("ToTwoToo", LintKind::WordChoice), Spelling);
        assert_eq!(IssueCategory::from_rule("TheirToTheyre", LintKind::Grammar), Grammar);
        assert_eq!(IssueCategory::from_rule("ItsContraction", LintKind::Punctuation), Grammar);
        // Other Repetition-kind rules ("that that" → "that which") stay hidden.
        assert_eq!(IssueCategory::from_rule("ThatWhich", LintKind::Repetition), Style);
        assert_eq!(IssueCategory::from_rule("SpellCheck", LintKind::Spelling), Spelling);
    }

    #[test]
    fn fragments_of_code_tokens_are_skipped() {
        // harper splits these tokens; the pieces look like prose alone.
        for (text, token) in [
            ("open src/lib.rs now", "src/lib.rs"),
            ("tail -f /tmp/tuipo.log", "-f"),
            ("tail -f /tmp/tuipo.log", "/tmp/tuipo.log"),
            ("git log --oneline -10", "--oneline"),
            ("rm -rf target", "-rf"),
            ("vim ~/.zshrc", "~/.zshrc"),
            ("python3 -m venv .venv now", ".venv"),
            ("cp -r src src.bak", "src.bak"),
            ("move the cursor with arrow CSIs", "CSIs"),
            ("colon-form SGR** breaks it", "SGR**"),
        ] {
            let start = text.find(token).expect("token in text");
            let end = start + token.len();
            let issues = check_raw(text);
            assert!(
                !issues.iter().any(|i| i.byte_start < end && i.byte_end > start),
                "lint on {token:?} in {text:?}: {issues:?}"
            );
        }
    }

    #[test]
    fn hyphenated_compounds_of_known_words_are_accepted() {
        let issues = check_raw("we re-run the multi-line non-empty check and it mis-parses");
        let spelling: Vec<_> = issues.iter().filter(|i| i.category == IssueCategory::Spelling).collect();
        assert!(spelling.is_empty(), "{spelling:?}");
        // A real typo inside a compound still counts.
        let issues = check_raw("we re-intrduced the bug");
        assert!(issues.iter().any(|i| i.category == IssueCategory::Spelling), "{issues:?}");
    }

    #[test]
    fn possessives_of_known_words_and_acronyms_are_accepted() {
        let issues = check_raw("the harper's dictionary and the PTY's buffer");
        let spelling: Vec<_> = issues.iter().filter(|i| i.category == IssueCategory::Spelling).collect();
        assert!(spelling.is_empty(), "{spelling:?}");
    }

    #[test]
    fn lone_letters_are_not_spelling_errors() {
        let issues = check_raw("it exits after 60 s of idle time");
        assert!(issues.iter().all(|i| i.word != "s"), "{issues:?}");
        // `That;s` is covered by its own whole-token lint.
        let issues = check_raw("That;s where it breaks");
        assert!(issues.iter().all(|i| i.word != "s"), "{issues:?}");
        assert!(by_rule(&issues, "WrongApostrophe").is_some(), "{issues:?}");
    }

    #[test]
    fn one_lint_per_misspelled_word() {
        // SpellCheck and SplitWords both flag "taht": one lint, best fix
        // first, and the noise split ("ta ht") gone.
        let issues = check_raw("fix taht bug");
        let on_taht: Vec<_> = issues.iter().filter(|i| i.word == "taht").collect();
        assert_eq!(on_taht.len(), 1, "{issues:?}");
        assert_eq!(on_taht[0].suggestions.first().map(String::as_str), Some("that"));
        assert!(!on_taht[0].suggestions.iter().any(|s| s.contains(' ')), "{:?}", on_taht[0]);
    }

    #[test]
    fn keyboard_slips_rank_first() {
        for (typo, fix) in [
            ("abuot", "about"),
            ("waht", "what"),
            ("adn", "and"),
            ("yuo", "you"),
            ("retrun", "return"),
            ("stirng", "string"),
            ("palce", "place"),
            ("comming", "coming"),
            ("teh", "the"),
            ("Teh", "The"),
        ] {
            let issues = check_raw(&format!("please {typo} now"));
            assert_eq!(top_fix(&issues, typo).as_deref(), Some(fix), "{typo}: {issues:?}");
        }
    }

    #[test]
    fn dropped_letter_fix_keeps_harpers_lead() {
        // "ned" is one swap from "end", but harper's "need" puts back a
        // dropped letter — a slip too, and here the intended word.
        let issues = check_raw("you don't ned to worry");
        assert_eq!(top_fix(&issues, "ned").as_deref(), Some("need"), "{issues:?}");
    }

    #[test]
    fn well_known_missing_space_fix_wins() {
        let issues = check_raw("there are alot of errors");
        assert_eq!(top_fix(&issues, "alot").as_deref(), Some("a lot"), "{issues:?}");
    }

    #[test]
    fn small_helpers() {
        assert!(next_word_is_complete("a important ", 1));
        assert!(next_word_is_complete("a important.", 1));
        assert!(!next_word_is_complete("a importa", 1));
        assert!(next_word_is_complete("a", 1));
        assert_eq!(trim_prose_punct("(\"teh\"),"), "teh");
        assert_eq!(trim_prose_punct("**bold**"), "bold");
        assert_eq!(trim_prose_punct("-rf"), "-rf");
        assert_eq!(trim_prose_punct(".venv"), ".venv");
        assert_eq!(trim_prose_punct("{placeholder}"), "{placeholder}");
        let text = "cp -r src src.bak now";
        let at = text.find("src src").unwrap();
        let (start, end) = token_bounds(text, at, at + "src src".len());
        assert_eq!(&text[start..end], "src src.bak");
        assert!(restores_dropped_letter("ned", "need"));
        assert!(restores_dropped_letter("wich", "which"));
        assert!(!restores_dropped_letter("abuot", "abbot"));
        assert!(!restores_dropped_letter("yuo", "yo"));
    }

    #[test]
    fn span_extraction_matches_word() {
        // Verify every issue's [byte_start..byte_end] slice equals its `word`.
        let text = "teh café 🦀 ones quikc";
        let issues = check(text);
        for issue in &issues {
            let slice = &text[issue.byte_start..issue.byte_end];
            assert_eq!(
                slice, issue.word,
                "byte slice {slice:?} did not match word {:?}",
                issue.word
            );
        }
    }
}
