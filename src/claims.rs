//! Extracting checkable claims from assistant prose.
//!
//! Only statements of completed fact are claims. Intentions ("let me run the tests"), questions,
//! and conditionals ("if the tests pass") are not, and matching them would flood the report with
//! noise. Every pattern here is therefore paired with a guard that rejects the hedged forms.

use regex::Regex;
use std::sync::OnceLock;

use crate::transcript::Transcript;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimKind {
    TestsPass,
    /// "all checks pass": a blanket claim naming no particular tool.
    ChecksPass,
    TypeCheckPasses,
    LintPasses,
    BuildPasses,
    Committed,
    Pushed,
    FileWritten,
}

impl ClaimKind {
    pub fn label(&self) -> &'static str {
        match self {
            ClaimKind::TestsPass => "tests pass",
            ClaimKind::ChecksPass => "checks pass",
            ClaimKind::TypeCheckPasses => "type check passes",
            ClaimKind::LintPasses => "lint passes",
            ClaimKind::BuildPasses => "build succeeds",
            ClaimKind::Committed => "changes committed",
            ClaimKind::Pushed => "changes pushed",
            ClaimKind::FileWritten => "file written",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Claim {
    pub kind: ClaimKind,
    /// Position in the transcript, for ordering against evidence.
    pub seq: usize,
    /// The sentence the claim was made in, trimmed for display.
    pub quote: String,
    /// True when made in the final message -- the summary the user is most likely to trust.
    pub in_summary: bool,
    /// For [`ClaimKind::FileWritten`], the path mentioned.
    pub subject: Option<String>,
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).expect("static regex"))
}

/// Sentences that describe an intention, a question, or a hypothetical make no claim.
fn is_hedged(sentence: &str) -> bool {
    static HEDGE: OnceLock<Regex> = OnceLock::new();
    let r = re(
        &HEDGE,
        r"(?i)\b(let me|i'?ll|i will|going to|next,? i|should i|shall i|can you|could you|need to|want to|planning to|about to|if |once |after |when |unless |until |make sure|ensure|verify that|check (?:that|whether|if)|to confirm|hopefully|assuming|in order to|so that|which (?:should|would)|would (?:be|make)|please )",
    );
    r.is_match(sentence) || sentence.trim_end().ends_with('?') || describes_something_else(sentence)
}

/// Recounting what some other thing did is not a claim about this session's work.
///
/// "The leak channel was `run.sh`, which did `git add` and pushed to `main` on every run"
/// explains a bug; it does not assert that the agent pushed anything.
fn describes_something_else(sentence: &str) -> bool {
    static DESC: OnceLock<Regex> = OnceLock::new();
    re(
        &DESC,
        r"(?i)\b(on every (?:single )?run|every single run|each run|each time|every time|used to|previously|historically|in the past|before this|which (?:did|does|was|were|had|has)|the (?:culprit|cause|reason|channel) )",
    )
    .is_match(sentence)
        || pushed_something_other_than_code(sentence)
}

/// "pushed the last item over" is a figure of speech, not a `git push`.
fn pushed_something_other_than_code(sentence: &str) -> bool {
    static METAPHOR: OnceLock<Regex> = OnceLock::new();
    re(
        &METAPHOR,
        r"(?i)\bpush(?:ed|es|ing)?\b[^.]{0,60}\b(over|past|beyond|across|through|into place|off|aside|back|down|up|onto|out of view|below the fold|to the (?:right|left)|out of|further)\b",
    )
    .is_match(sentence)
}

/// Sentences that assert the opposite are not positive claims.
///
/// A negative word does not always carry a negative meaning: "no warnings" and "without errors"
/// are exactly the phrasings a successful run gets described in. Those are neutralised before the
/// negation test runs, or every clean result would be discarded.
fn is_negated(sentence: &str) -> bool {
    static POSITIVE_NEGATIVES: OnceLock<Regex> = OnceLock::new();
    let cleaned = re(
        &POSITIVE_NEGATIVES,
        r"(?i)\bno\s+(?:errors?|issues?|warnings?|failures?|problems?|regressions?|complaints?)\b|\bwithout\s+(?:errors?|issues?|warnings?|failures?|problems?)\b|\bzero\s+(?:errors?|issues?|failures?|warnings?)\b|\bno\s+longer\s+fail",
    )
    .replace_all(sentence, " ");

    static NEG: OnceLock<Regex> = OnceLock::new();
    re(
        &NEG,
        r"(?i)\b(do(?:es)?n'?t|did\s+not|didn'?t|is\s?n'?t|are\s?n'?t|was\s?n'?t|were\s?n'?t|not|no|nowhere|none|never|fail(?:s|ed|ing|ure|ures)?|broken|breaks?|error(?:s)?|cannot|can'?t|couldn'?t|unable|still|pending|remains?)\b",
    )
    .is_match(&cleaned)
}

/// Remove inline code spans and emphasis markers from a line of prose.
fn strip_markup(line: &str) -> String {
    static CODE: OnceLock<Regex> = OnceLock::new();
    let without_code = re(&CODE, r"`[^`]*`").replace_all(line, " ");
    without_code.replace("**", "").replace("__", "")
}

/// Split prose into sentences.
///
/// Splitting happens only at a full stop that is followed by whitespace or the end of the line,
/// so paths and version numbers (`src/parser.rs`, `v1.2.3`) survive intact -- they are frequently
/// the subject of the very claims being extracted.
fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;

    for line in text.lines() {
        let trimmed = line.trim();
        // Fenced blocks quote command output; a claim only counts when the agent asserts it.
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || trimmed.is_empty() {
            continue;
        }
        // An indented block is also quoted output rather than prose.
        if line.starts_with("    ") || line.starts_with('\t') {
            continue;
        }

        // Strip inline code spans and emphasis before matching. A sentence *about* checking,
        // quoting `> tsc -b` or **bold** fragments of output, is discussion rather than a
        // claim, and the markup is what made it read like one.
        let trimmed = strip_markup(trimmed);
        let trimmed = trimmed.as_str();

        let chars: Vec<char> = trimmed.chars().collect();
        let mut cur = String::new();
        for (i, &ch) in chars.iter().enumerate() {
            cur.push(ch);
            let ends_sentence = matches!(ch, '.' | '!' | '。')
                && chars.get(i + 1).is_none_or(|next| next.is_whitespace());
            if ends_sentence && cur.trim().len() > 8 {
                out.push(std::mem::take(&mut cur));
            }
        }
        if !cur.trim().is_empty() {
            out.push(cur);
        }
    }

    out.into_iter()
        .map(|s| {
            s.trim()
                .trim_start_matches(['-', '*', '#', ' '])
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

struct Pattern {
    kind: ClaimKind,
    regex: &'static str,
    cell: &'static OnceLock<Regex>,
}

fn all_patterns() -> Vec<Pattern> {
    static TESTS: OnceLock<Regex> = OnceLock::new();
    static TYPES: OnceLock<Regex> = OnceLock::new();
    static LINT: OnceLock<Regex> = OnceLock::new();
    static BUILD: OnceLock<Regex> = OnceLock::new();
    static CHECKS: OnceLock<Regex> = OnceLock::new();
    static COMMIT: OnceLock<Regex> = OnceLock::new();
    static PUSH: OnceLock<Regex> = OnceLock::new();

    vec![
        Pattern {
            kind: ClaimKind::TestsPass,
            // "all tests pass", "the test suite is green", "tests passing", "42 tests pass".
            // "green" is only a verdict next to the thing it describes: a paper award called
            // "Test of Time" rendered with a "green gradient banner" is not a passing suite.
            regex: r"(?i)\b(?:all |the |\d+ )*tests?(?: suite)?\b[^.]{0,30}\b(pass(?:es|ed|ing)?|succeed(?:s|ed)?)\b|\b(?:tests?|test suite|suite|build|ci)\b\s*(?:is|are|all|now|still|were)?\s*green\b|\ball green\b",
            cell: &TESTS,
        },
        Pattern {
            kind: ClaimKind::ChecksPass,
            // Deliberately vague on purpose: "checks" may mean tests, lint, types, CI or all
            // of them, so this is verified against whatever ran rather than against one tool.
            regex: r"(?i)\ball (?:the )?checks?\b[^.]{0,20}\bpass(?:es|ed|ing)?\b|\beverything (?:is |was )?green\b|\ball green\b",
            cell: &CHECKS,
        },
        Pattern {
            kind: ClaimKind::TypeCheckPasses,
            regex: r"(?i)\b(mypy|pyright|tsc|type[ -]?check(?:s|ing)?|types?)\b[^.]{0,30}\b(pass(?:es|ed|ing)?|clean|clear|happy|no (?:errors|issues))\b",
            cell: &TYPES,
        },
        Pattern {
            kind: ClaimKind::LintPasses,
            regex: r"(?i)\b(lint(?:er|ing)?|ruff|eslint|clippy|flake8)\b[^.]{0,30}\b(pass(?:es|ed|ing)?|clean|clear|happy|no (?:errors|issues|warnings))\b",
            cell: &LINT,
        },
        Pattern {
            kind: ClaimKind::BuildPasses,
            regex: r"(?i)\b(build|compil(?:es|ed|ation))\b[^.]{0,25}\b(succe(?:eds|eded|ssful)|pass(?:es|ed)?|clean(?:ly)?|works?|fine)\b",
            cell: &BUILD,
        },
        Pattern {
            kind: ClaimKind::Committed,
            regex: r"(?i)\b(i(?:'ve| have)? committed|committed (?:the|these|all|it|them|everything)|(?:changes|work|fix|it) (?:is|are|were|has been|have been) committed|created (?:a|the) commit|commit(?:ted)? (?:and pushed|locally))\b",
            cell: &COMMIT,
        },
        Pattern {
            kind: ClaimKind::Pushed,
            regex: r"(?i)\b(i(?:'ve| have)? pushed|pushed (?:the|these|all|it|them|to)|(?:changes|work|branch|commits?) (?:is|are|were|has been|have been) pushed)\b",
            cell: &PUSH,
        },
    ]
}

/// Domain suffixes that make a token a web address rather than a file.
const TLDS: &[&str] = &[
    "com", "org", "net", "io", "dev", "ai", "co", "sh", "me", "app", "gov", "edu", "uk", "ca",
    "html",
];

/// Paths mentioned as having been created or written.
///
/// URLs are the main source of false matches here -- "PR created: https://github.com/..." parses
/// as a claim to have written a file called `github.com` -- so anything that looks like an address
/// is discarded.
fn file_claims(sentence: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let r = re(
        &RE,
        r"(?i)\b(?:created|added|wrote|written|updated|modified)\b[^.]{0,60}?\b([\w./-]+\.[a-z]{1,6})\b",
    );

    let has_url = sentence.contains("://") || sentence.contains("www.");

    r.captures_iter(sentence)
        .map(|c| c[1].to_string())
        .filter(|p| {
            if p.starts_with("e.g") || !p.contains('.') {
                return false;
            }
            let ext = p.rsplit('.').next().unwrap_or("").to_lowercase();
            // A bare `example.com` is an address; `docs/index.html` is a file.
            if TLDS.contains(&ext.as_str()) && !p.contains('/') {
                return false;
            }
            // Inside a sentence containing a URL, a slashed token is most likely part of it.
            if has_url && sentence.contains(&format!("/{p}")) && TLDS.contains(&ext.as_str()) {
                return false;
            }
            true
        })
        .collect()
}

/// Extract every claim in the transcript.
pub fn extract(transcript: &Transcript) -> Vec<Claim> {
    let patterns = all_patterns();
    let mut claims = Vec::new();

    for msg in &transcript.assistant_texts {
        for sentence in sentences(&msg.text) {
            if is_hedged(&sentence) {
                continue;
            }
            let negated = is_negated(&sentence);

            for p in &patterns {
                if !re(p.cell, p.regex).is_match(&sentence) {
                    continue;
                }
                // "tests don't pass yet" is an honest report, not a claim to verify.
                if negated {
                    continue;
                }
                claims.push(Claim {
                    kind: p.kind,
                    seq: msg.seq,
                    quote: truncate(&sentence, 160),
                    in_summary: msg.is_last,
                    subject: None,
                });
            }

            if !negated {
                for path in file_claims(&sentence) {
                    claims.push(Claim {
                        kind: ClaimKind::FileWritten,
                        seq: msg.seq,
                        quote: truncate(&sentence, 160),
                        in_summary: msg.is_last,
                        subject: Some(path),
                    });
                }
            }
        }
    }

    dedupe(claims)
}

/// Keep the last occurrence of each (kind, subject): later claims supersede earlier ones, and the
/// summary is what the user actually reads.
fn dedupe(mut claims: Vec<Claim>) -> Vec<Claim> {
    claims.sort_by_key(|c| c.seq);
    let mut seen: Vec<(ClaimKind, Option<String>)> = Vec::new();
    let mut out: Vec<Claim> = Vec::new();
    for c in claims.into_iter().rev() {
        let key = (c.kind, c.subject.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(c);
    }
    out.reverse();
    out
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Transcript;

    fn claims_from(text: &str) -> Vec<Claim> {
        let line = serde_json::json!({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": text}]}
        });
        extract(&Transcript::parse_str(&line.to_string()))
    }

    fn kinds(text: &str) -> Vec<ClaimKind> {
        claims_from(text).into_iter().map(|c| c.kind).collect()
    }

    #[test]
    fn detects_test_claims() {
        assert!(kinds("All tests pass.").contains(&ClaimKind::TestsPass));
        assert!(kinds("The test suite is green.").contains(&ClaimKind::TestsPass));
        assert!(kinds("All 42 tests passing now.").contains(&ClaimKind::TestsPass));
    }

    #[test]
    fn ignores_intentions_and_questions() {
        assert!(kinds("Let me run the tests to see if they pass.").is_empty());
        assert!(kinds("I'll make sure all tests pass.").is_empty());
        assert!(kinds("Do all tests pass?").is_empty());
        assert!(kinds("Once the tests pass, I will commit.").is_empty());
    }

    #[test]
    fn recognises_all_checks_pass() {
        // Seen verbatim in two separate sessions and extracted from neither. They name no
        // particular tool, so they get their own kind and are checked against whatever ran.
        assert!(kinds("All checks pass.").contains(&ClaimKind::ChecksPass));
        assert!(
            kinds("All checks pass (369 tests, ruff, mypy, pre-commit).")
                .contains(&ClaimKind::ChecksPass)
        );
        assert!(kinds("Everything is green.").contains(&ClaimKind::ChecksPass));
        // Still not a claim when it is something the agent intends to do.
        assert!(kinds("I'll merge once all checks pass.").is_empty());
    }

    #[test]
    fn green_needs_to_describe_the_tests() {
        // Regression: a conference award named "Test of Time" shown with a green banner was
        // read as a passing suite.
        assert!(kinds("Test of Time → green gradient banner, laid out as a highlight.").is_empty());
        assert!(kinds("The tests are green.").contains(&ClaimKind::TestsPass));
        assert!(kinds("All green.").contains(&ClaimKind::TestsPass));
    }

    #[test]
    fn ignores_figurative_pushes() {
        // Regression: "pushed the last item over" was read as a git push.
        assert!(
            kinds("The accuracy edits added a few lines and pushed the last item over.").is_empty()
        );
        assert!(kinds("Pushed to origin.").contains(&ClaimKind::Pushed));
    }

    #[test]
    fn ignores_descriptions_of_other_things() {
        // Regression: explaining a bug in run.sh was read as the agent claiming it pushed.
        assert!(kinds(
            "The leak channel was `run.sh`, which did `git add \"$OUTPUT\"` and pushed to `main` on every single run."
        )
        .is_empty());
        assert!(kinds("The old script used to push to main automatically.").is_empty());
        // A genuine first-person claim must still register.
        assert!(kinds("Pushed to origin.").contains(&ClaimKind::Pushed));
    }

    #[test]
    fn ignores_negated_reports() {
        assert!(kinds("Two tests still fail.").is_empty());
        assert!(kinds("The tests do not pass yet.").is_empty());
    }

    #[test]
    fn detects_commit_and_push() {
        assert!(kinds("I've committed the changes.").contains(&ClaimKind::Committed));
        assert!(kinds("Pushed to origin.").contains(&ClaimKind::Pushed));
    }

    #[test]
    fn detects_lint_and_typecheck() {
        assert!(kinds("mypy is clean.").contains(&ClaimKind::TypeCheckPasses));
        assert!(kinds("Ruff passes with no warnings.").contains(&ClaimKind::LintPasses));
    }

    #[test]
    fn detects_file_writes_with_subject() {
        let c = claims_from("Created src/parser.rs with the new logic.");
        let f = c.iter().find(|c| c.kind == ClaimKind::FileWritten).unwrap();
        assert_eq!(f.subject.as_deref(), Some("src/parser.rs"));
    }

    #[test]
    fn skips_code_blocks() {
        assert!(kinds("```\nall tests pass\n```").is_empty());
        // Multi-line fences must stay closed across every line they span.
        assert!(kinds("```sh\n$ pytest\nall tests pass\nmore output\n```").is_empty());
    }

    #[test]
    fn does_not_treat_urls_as_written_files() {
        // Regression: "github.com" used to be extracted as a file the agent claimed to write.
        let c = claims_from("PR created: https://github.com/acme/repo/pull/206");
        assert!(
            !c.iter().any(|c| c.kind == ClaimKind::FileWritten),
            "URL should not be a file claim: {c:?}"
        );
    }

    #[test]
    fn keeps_file_paths_intact_across_sentence_splitting() {
        // Regression: splitting on every '.' cut paths like `src/parser.rs` in half.
        let c = claims_from("Created src/parser.rs with the new logic. It works.");
        let f = c.iter().find(|c| c.kind == ClaimKind::FileWritten).unwrap();
        assert_eq!(f.subject.as_deref(), Some("src/parser.rs"));
    }

    #[test]
    fn positive_phrasings_with_negative_words_still_count() {
        // "no errors" describes success; it must not be read as a negated claim.
        assert!(kinds("Ruff passes with no warnings.").contains(&ClaimKind::LintPasses));
        assert!(kinds("mypy is clean with zero errors.").contains(&ClaimKind::TypeCheckPasses));
    }

    #[test]
    fn deduplicates_repeated_claims() {
        let c = claims_from("All tests pass. Tests pass. All tests pass.");
        assert_eq!(
            c.iter().filter(|c| c.kind == ClaimKind::TestsPass).count(),
            1
        );
    }
}
