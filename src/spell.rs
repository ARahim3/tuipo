//! Spell/grammar engine. Wraps `harper-core` and:
//!   - Converts harper's char-offset spans to byte offsets (matching our
//!     buffer's cursor model).
//!   - Filters out lints on code-shaped tokens (paths, snake_case, --flags,
//!     CamelCase, ALLCAPS, URLs) so we don't scream at every `useState`.
//!   - Collapses harper's 20-variant `LintKind` into a coarser
//!     [`IssueCategory`] for UI use.

use harper_core::Dialect;
use harper_core::Document;
use harper_core::linting::{LintGroup, LintKind, Linter, Suggestion};
use harper_core::parsers::PlainEnglish;
use harper_core::spell::FstDictionary;

use crate::dict::CustomDict;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IssueCategory {
    Spelling,
    Grammar,
    Style,
    Other,
}

impl IssueCategory {
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
}

pub struct SpellChecker {
    linter: LintGroup,
    custom: CustomDict,
}

impl SpellChecker {
    pub fn new() -> Self {
        Self::with_custom(CustomDict::from_default_path())
    }

    pub fn with_custom(custom: CustomDict) -> Self {
        let dict = FstDictionary::curated();
        let linter = LintGroup::new_curated(dict, Dialect::American);
        Self { linter, custom }
    }

    pub fn check(&mut self, text: &str) -> Vec<SpellIssue> {
        if text.trim().is_empty() {
            return Vec::new();
        }
        let doc = Document::new_curated(text, &PlainEnglish);
        let lints = self.linter.lint(&doc);

        // Build a char-offset → byte-offset prefix sum once, so each lint's
        // span conversion is O(1) instead of O(n).
        let mut char_to_byte: Vec<usize> = Vec::with_capacity(text.len() + 1);
        for (b, _) in text.char_indices() {
            char_to_byte.push(b);
        }
        char_to_byte.push(text.len()); // sentinel for end-of-string

        // Char view of the text, for extracting span content.
        let chars: Vec<char> = text.chars().collect();

        lints
            .into_iter()
            .filter_map(|lint| {
                let span = lint.span;
                let char_start = span.start;
                let char_end = span.end;
                let byte_start = *char_to_byte.get(char_start)?;
                let byte_end = *char_to_byte.get(char_end)?;
                let word: String = chars
                    .get(char_start..char_end)
                    .map(|cs| cs.iter().collect())
                    .unwrap_or_default();

                if looks_like_code(&word, text, byte_start, byte_end) {
                    return None;
                }
                if self.custom.contains(&word) {
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
                    category: IssueCategory::from_kind(lint.lint_kind),
                    priority: lint.priority,
                })
            })
            .collect()
    }
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
/// time. Handles two shapes:
///
/// - **Single-token spelling lints** (e.g. `useState`, `src/main.rs`).
///   Run all the heuristics against the whole word.
/// - **Multi-token grammar lints** (e.g. `the API is`, `useState should`).
///   Grammar lints often span phrases; if *any* token in the span is
///   code-shaped we skip the whole lint — we'd rather miss a grammar
///   warning around mixed code/prose than paint underlines under
///   identifiers.
fn looks_like_code(word: &str, full_text: &str, byte_start: usize, byte_end: usize) -> bool {
    if word.is_empty() {
        return true;
    }
    // Wrapped in backticks: definitely code. Applies to the whole span
    // regardless of token count.
    if byte_start > 0 && byte_end < full_text.len() {
        let prev = full_text[..byte_start].chars().next_back();
        let next = full_text[byte_end..].chars().next();
        if prev == Some('`') && next == Some('`') {
            return true;
        }
    }
    // Multi-token span (grammar lint shape). If any single token looks
    // like code, the whole lint gets skipped.
    if word.contains(char::is_whitespace) {
        return word
            .split_whitespace()
            .any(looks_like_code_token);
    }
    looks_like_code_token(word)
}

/// Token-level shape check. See `looks_like_code` for the wrapping
/// logic that handles backtick context and multi-token spans.
fn looks_like_code_token(word: &str) -> bool {
    if word.is_empty() {
        return true;
    }
    let chars: Vec<char> = word.chars().collect();

    // Contains structural punctuation/symbols typical of identifiers/paths/URLs.
    if word.contains('/')
        || word.contains('\\')
        || word.contains('_')
        || word.contains('@')
        || word.contains('#')
        || word.contains('$')
        || word.contains(':')
    {
        return true;
    }

    // Starts with '-' (flag) or contains digits adjacent to letters (versions, IDs).
    if word.starts_with('-') {
        return true;
    }
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

    // All-caps two chars or more — likely an acronym/constant.
    if chars.len() >= 2 && chars.iter().all(|c| c.is_ascii_uppercase()) {
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
