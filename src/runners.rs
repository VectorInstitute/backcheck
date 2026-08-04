//! Recognising verification commands and reading their outcome from their output.
//!
//! Claude Code transcripts do not record exit codes, so a command's success has to be recovered
//! from what it printed. Each supported tool gets a parser for its summary line -- pytest's
//! `5 passed, 1 failed`, cargo's `test result: FAILED`, jest's `Tests: 1 failed, 4 passed`, and
//! so on -- with a conservative textual fallback for everything else.
//!
//! The parsers deliberately return [`Outcome::Unknown`] rather than guessing. A wrong "verified"
//! is far more damaging than an honest "could not tell".

use regex::Regex;
use std::sync::OnceLock;

/// What kind of check a command performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    Test,
    TypeCheck,
    Lint,
    Build,
}

impl CheckKind {
    pub fn label(&self) -> &'static str {
        match self {
            CheckKind::Test => "test",
            CheckKind::TypeCheck => "type check",
            CheckKind::Lint => "lint",
            CheckKind::Build => "build",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed,
    /// The command ran but its result could not be determined from the output.
    Unknown,
    /// The command never finished: interrupted, timed out, or blocked.
    DidNotComplete,
}

/// Reasons a run does not fully back a blanket "everything passes" claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caveat {
    /// Only a subset of tests ran (`-k`, `--only`, an explicit file or test id).
    SubsetOnly(String),
    /// The run stopped at the first failure, so later tests never executed.
    StopsEarly(String),
    /// Failure was swallowed by the shell (`|| true`, `; exit 0`, `set +e`).
    FailureSuppressed(String),
    /// Output was piped through something that can hide the summary.
    OutputFiltered(String),
    /// The runner reported that it collected no tests at all.
    NoTestsRan,
}

impl Caveat {
    pub fn describe(&self) -> String {
        match self {
            Caveat::SubsetOnly(w) => format!("only a subset of tests ran ({w})"),
            Caveat::StopsEarly(w) => format!("run stops at first failure ({w})"),
            Caveat::FailureSuppressed(w) => format!("a non-zero exit would be hidden by `{w}`"),
            Caveat::OutputFiltered(w) => format!("output was filtered through `{w}`"),
            Caveat::NoTestsRan => "the runner collected no tests".to_string(),
        }
    }
}

/// A single recognised check command and what became of it.
#[derive(Debug, Clone)]
pub struct CheckRun {
    pub seq: usize,
    pub kind: CheckKind,
    /// The tool that ran, e.g. `pytest`, `cargo test`.
    pub runner: String,
    pub command: String,
    pub outcome: Outcome,
    pub caveats: Vec<Caveat>,
    /// Counts when the runner reported them.
    pub passed: Option<u32>,
    pub failed: Option<u32>,
    /// A short line of output supporting the verdict, shown to the user as proof.
    pub evidence_line: Option<String>,
}

impl CheckRun {
    /// A clean pass is a pass with nothing qualifying it.
    pub fn is_clean_pass(&self) -> bool {
        self.outcome == Outcome::Passed && self.caveats.is_empty()
    }
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).expect("static regex"))
}

/// Split a shell line into segments that each start a fresh command.
///
/// Pipelines are kept intact: `pytest | tail` is one command whose output happens to be filtered.
pub fn split_segments(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut in_single, mut in_double) = (false, false);
    // Iterate by character: commands routinely contain multi-byte text (paths, heredoc prose),
    // and indexing them by byte would split a character in half.
    let mut chars = command.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            _ => {}
        }

        if !in_single && !in_double {
            // `&&` and `||` start a new command; a lone `|` is a pipe and keeps the segment.
            if (c == '&' || c == '|') && chars.peek() == Some(&c) {
                chars.next();
                out.push(std::mem::take(&mut cur));
                continue;
            }
            if c == ';' || c == '\n' {
                out.push(std::mem::take(&mut cur));
                continue;
            }
        }
        cur.push(c);
    }

    out.push(cur);
    out.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Reduce a command to the tool that actually runs.
///
/// Real sessions rarely invoke a runner by its bare name. It arrives behind an environment
/// (`uv run pytest`), an interpreter (`python -m pytest`), or -- most commonly of all -- an
/// absolute or virtualenv-relative path (`.venv/bin/python -m pytest`, `./node_modules/.bin/jest`).
/// Each of those has to collapse to `pytest` or `jest`, or the run is invisible and an honest
/// claim gets reported as unsupported.
fn strip_invocation(segment: &str) -> String {
    let mut s = segment.trim().to_string();

    loop {
        let before = s.clone();

        // Drop a directory prefix on the executable: `.venv-test/bin/python` -> `python`.
        if let Some(rest) = s.strip_prefix("./").or(Some(s.as_str())) {
            let mut parts = rest.splitn(2, char::is_whitespace);
            if let Some(head) = parts.next() {
                if head.contains('/') || head.contains('\\') {
                    let base = head.rsplit(['/', '\\']).next().unwrap_or(head);
                    s = match parts.next() {
                        Some(tail) => format!("{base} {tail}"),
                        None => base.to_string(),
                    };
                }
            }
        }

        // Drop wrappers that delegate to the real tool.
        for prefix in [
            "uv run ",
            "uvx ",
            "poetry run ",
            "pipenv run ",
            "pdm run ",
            "hatch run ",
            "rye run ",
            "conda run ",
            "npx ",
            "pnpm exec ",
            "pnpm dlx ",
            "yarn dlx ",
            "yarn ",
            "pnpm ",
            "bunx ",
            "time ",
            "sudo ",
            "env ",
            "nice ",
        ] {
            if let Some(rest) = s.strip_prefix(prefix) {
                s = rest.trim_start().to_string();
            }
        }

        // `python -m pytest`, `python3.12 -m pytest`, `py -m pytest`.
        static PY_M: OnceLock<Regex> = OnceLock::new();
        let py = re(&PY_M, r"^(?:python[0-9.]*|py|pypy[0-9.]*)\s+-m\s+");
        if let Some(m) = py.find(&s) {
            s = s[m.end()..].trim_start().to_string();
        }

        if s == before {
            return s;
        }
    }
}

/// Identify the check a command segment performs, if any.
pub fn classify(segment: &str) -> Option<(CheckKind, String)> {
    // A JavaScript package manager running its `test` script. Matched on the original segment
    // rather than a stripped one: after stripping, `yarn test` and the shell builtin `test -f`
    // are indistinguishable, and treating `test -f` as a test run would hide a missing suite.
    static JS_TEST: OnceLock<Regex> = OnceLock::new();
    if re(&JS_TEST, r"(?i)\b(?:npm|yarn|pnpm|bun)\s+(?:run\s+)?test\b").is_match(segment) {
        return Some((CheckKind::Test, "npm test".into()));
    }

    let s = strip_invocation(segment);
    let s = s.as_str();
    let lower = s.to_lowercase();
    let head = lower.split_whitespace().next().unwrap_or("");

    // Test runners.
    let test_runners: &[(&str, &str)] = &[
        ("pytest", "pytest"),
        ("tox", "tox"),
        ("nox", "nox"),
        ("unittest", "unittest"),
        ("jest", "jest"),
        ("vitest", "vitest"),
        ("mocha", "mocha"),
        ("ava", "ava"),
        ("rspec", "rspec"),
        ("phpunit", "phpunit"),
        ("ctest", "ctest"),
    ];
    for (needle, name) in test_runners {
        if head == *needle {
            return Some((CheckKind::Test, (*name).to_string()));
        }
    }
    if lower.starts_with("cargo test") || lower.starts_with("cargo nextest") {
        return Some((CheckKind::Test, "cargo test".into()));
    }
    if lower.starts_with("go test") {
        return Some((CheckKind::Test, "go test".into()));
    }
    if lower.starts_with("dotnet test") {
        return Some((CheckKind::Test, "dotnet test".into()));
    }
    if lower.starts_with("mvn ") && lower.contains("test") {
        return Some((CheckKind::Test, "maven".into()));
    }
    if (lower.starts_with("gradle ") || lower.starts_with("./gradlew ")) && lower.contains("test") {
        return Some((CheckKind::Test, "gradle".into()));
    }
    if lower.starts_with("bun test") {
        return Some((CheckKind::Test, "bun test".into()));
    }
    if lower.starts_with("make test") || lower.starts_with("make check") {
        return Some((CheckKind::Test, "make test".into()));
    }

    // Type checkers.
    for (needle, name) in [("mypy", "mypy"), ("pyright", "pyright"), ("tsc", "tsc")] {
        if head == needle {
            return Some((CheckKind::TypeCheck, name.to_string()));
        }
    }
    if lower.starts_with("cargo check") {
        return Some((CheckKind::TypeCheck, "cargo check".into()));
    }

    // Linters.
    for (needle, name) in [
        ("ruff", "ruff"),
        ("eslint", "eslint"),
        ("flake8", "flake8"),
        ("pylint", "pylint"),
        ("golangci-lint", "golangci-lint"),
        ("biome", "biome"),
    ] {
        if head == needle {
            return Some((CheckKind::Lint, name.to_string()));
        }
    }
    if lower.starts_with("cargo clippy") {
        return Some((CheckKind::Lint, "clippy".into()));
    }
    if lower.starts_with("cargo fmt") || lower.starts_with("gofmt") {
        return Some((CheckKind::Lint, "format check".into()));
    }

    // Builds.
    if lower.starts_with("cargo build") {
        return Some((CheckKind::Build, "cargo build".into()));
    }
    if lower.starts_with("go build") {
        return Some((CheckKind::Build, "go build".into()));
    }
    if lower.starts_with("npm run build") || lower.starts_with("run build") {
        return Some((CheckKind::Build, "npm build".into()));
    }

    None
}

/// Shell constructs anywhere in the command line that would swallow a non-zero exit.
///
/// These are checked against the whole command rather than a single segment: by the time the line
/// has been split, the `|| true` that neutralises a failing `pytest` is a segment of its own.
fn suppression_caveats(full_command: &str) -> Vec<Caveat> {
    let lower = full_command.to_lowercase();
    let mut out = Vec::new();
    for token in [
        "|| true",
        "|| echo",
        "|| :",
        "; true",
        "set +e",
        "|| exit 0",
    ] {
        if lower.contains(token) {
            out.push(Caveat::FailureSuppressed(token.to_string()));
        }
    }
    out
}

/// Flags and shell tricks that qualify a run.
fn caveats_for(segment: &str, kind: CheckKind) -> Vec<Caveat> {
    let mut out = Vec::new();
    let lower = segment.to_lowercase();

    if kind == CheckKind::Test {
        for token in [" -k ", " --only", " -run ", " --grep", " -t "] {
            if lower.contains(token) {
                out.push(Caveat::SubsetOnly(token.trim().to_string()));
                break;
            }
        }
        for token in [" -x", " --exitfirst", " --maxfail", " --bail", " -ff"] {
            if lower.contains(token) {
                out.push(Caveat::StopsEarly(token.trim().to_string()));
                break;
            }
        }
    }
    // `| tail` keeps the summary of most runners, but `head`/`grep` can drop it entirely.
    for token in ["| head", "|head", "| grep", "|grep"] {
        if lower.contains(token) {
            out.push(Caveat::OutputFiltered(token.trim().to_string()));
            break;
        }
    }
    out
}

/// Does the command name specific test files or test ids, rather than a whole suite?
fn targets_specific_tests(segment: &str) -> Option<Caveat> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let r = re(
        &RE,
        r"(?i)\s([\w./\\-]*test[\w./\\-]*\.(py|rs|ts|tsx|js|jsx|go|rb|java)(::[\w:]+)?)",
    );
    r.captures(segment)
        .map(|c| Caveat::SubsetOnly(c[1].to_string()))
}

/// Read a runner's outcome out of its output.
fn parse_outcome(
    runner: &str,
    kind: CheckKind,
    output: &str,
) -> (Outcome, Option<u32>, Option<u32>, Option<String>) {
    let find = |pat: &str, cell: &'static OnceLock<Regex>| -> Option<Vec<String>> {
        re(cell, pat).captures(output).map(|c| {
            c.iter()
                .map(|m| m.map(|m| m.as_str().to_string()).unwrap_or_default())
                .collect()
        })
    };
    let line_containing = |needle: &str| -> Option<String> {
        output
            .lines()
            .rev()
            .find(|l| l.to_lowercase().contains(needle))
            .map(|l| l.trim().to_string())
    };

    match runner {
        "pytest" | "tox" | "nox" | "unittest" => {
            static NO_TESTS: OnceLock<Regex> = OnceLock::new();
            if re(&NO_TESTS, r"(?i)no tests ran|collected 0 items").is_match(output) {
                return (Outcome::Failed, Some(0), None, line_containing("no tests"));
            }
            static SUMMARY: OnceLock<Regex> = OnceLock::new();
            let passed = find(r"(\d+) passed", &SUMMARY).and_then(|c| c[1].parse().ok());
            static FAILED: OnceLock<Regex> = OnceLock::new();
            let failed: Option<u32> =
                find(r"(\d+) (?:failed|error)", &FAILED).and_then(|c| c[1].parse().ok());
            let ev = line_containing("passed").or_else(|| line_containing("failed"));
            match (passed, failed) {
                (_, Some(f)) if f > 0 => (Outcome::Failed, passed, Some(f), ev),
                (Some(p), _) if p > 0 => (Outcome::Passed, Some(p), Some(0), ev),
                _ => (Outcome::Unknown, passed, failed, ev),
            }
        }
        "cargo test" => {
            static RES: OnceLock<Regex> = OnceLock::new();
            if let Some(c) = find(
                r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed",
                &RES,
            ) {
                let passed = c[2].parse().ok();
                let failed: Option<u32> = c[3].parse().ok();
                let ev = line_containing("test result:");
                return if c[1] == "ok" && failed == Some(0) {
                    (Outcome::Passed, passed, failed, ev)
                } else {
                    (Outcome::Failed, passed, failed, ev)
                };
            }
            if output.contains("error[") || output.contains("error: could not compile") {
                return (Outcome::Failed, None, None, line_containing("error"));
            }
            (Outcome::Unknown, None, None, None)
        }
        "go test" => {
            if output
                .lines()
                .any(|l| l.starts_with("FAIL") || l.contains("--- FAIL"))
            {
                return (Outcome::Failed, None, None, line_containing("fail"));
            }
            if output
                .lines()
                .any(|l| l.starts_with("ok ") || l.starts_with("PASS"))
            {
                return (Outcome::Passed, None, None, line_containing("ok "));
            }
            (Outcome::Unknown, None, None, None)
        }
        "jest" | "vitest" => {
            static T: OnceLock<Regex> = OnceLock::new();
            if let Some(c) = find(
                r"Tests:\s+(?:(\d+) failed,\s+)?(?:\d+ skipped,\s+)?(\d+) passed",
                &T,
            ) {
                let failed: u32 = c[1].parse().unwrap_or(0);
                let passed: Option<u32> = c[2].parse().ok();
                let ev = line_containing("tests:");
                return if failed > 0 {
                    (Outcome::Failed, passed, Some(failed), ev)
                } else {
                    (Outcome::Passed, passed, Some(0), ev)
                };
            }
            if output.contains("FAIL ") {
                return (Outcome::Failed, None, None, line_containing("fail "));
            }
            if output.contains("PASS ") {
                return (Outcome::Passed, None, None, line_containing("pass "));
            }
            (Outcome::Unknown, None, None, None)
        }
        "mocha" | "ava" | "rspec" => {
            static P: OnceLock<Regex> = OnceLock::new();
            static F: OnceLock<Regex> = OnceLock::new();
            let passed: Option<u32> =
                find(r"(\d+) (?:passing|examples?)", &P).and_then(|c| c[1].parse().ok());
            let failed: Option<u32> =
                find(r"(\d+) (?:failing|failures?)", &F).and_then(|c| c[1].parse().ok());
            let ev = line_containing("passing").or_else(|| line_containing("failure"));
            match (passed, failed) {
                (_, Some(f)) if f > 0 => (Outcome::Failed, passed, Some(f), ev),
                (Some(p), _) if p > 0 => (Outcome::Passed, Some(p), failed.or(Some(0)), ev),
                _ => (Outcome::Unknown, passed, failed, ev),
            }
        }
        "mypy" => {
            if output.contains("Success: no issues found") {
                return (Outcome::Passed, None, Some(0), line_containing("success:"));
            }
            static E: OnceLock<Regex> = OnceLock::new();
            if let Some(c) = find(r"Found (\d+) error", &E) {
                return (
                    Outcome::Failed,
                    None,
                    c[1].parse().ok(),
                    line_containing("found "),
                );
            }
            (Outcome::Unknown, None, None, None)
        }
        "ruff" => {
            if output.contains("All checks passed") || output.contains("files already formatted") {
                return (Outcome::Passed, None, Some(0), line_containing("passed"));
            }
            static E: OnceLock<Regex> = OnceLock::new();
            if let Some(c) = find(r"Found (\d+) error", &E) {
                return (
                    Outcome::Failed,
                    None,
                    c[1].parse().ok(),
                    line_containing("found "),
                );
            }
            (Outcome::Unknown, None, None, None)
        }
        "clippy" | "cargo check" | "cargo build" => {
            static E: OnceLock<Regex> = OnceLock::new();
            if let Some(c) = find(r"error(?:\[E\d+\])?:", &E) {
                let _ = c;
                return (Outcome::Failed, None, None, line_containing("error"));
            }
            if output.contains("Finished") || output.contains("Compiling") {
                return (Outcome::Passed, None, Some(0), line_containing("finished"));
            }
            (Outcome::Unknown, None, None, None)
        }
        "tsc" | "pyright" | "eslint" | "flake8" | "pylint" | "golangci-lint" | "biome"
        | "format check" | "npm build" | "go build" => generic_outcome(output, kind),
        _ => generic_outcome(output, kind),
    }
}

/// Conservative fallback: only call it a pass or a fail on unambiguous signals.
fn generic_outcome(
    output: &str,
    kind: CheckKind,
) -> (Outcome, Option<u32>, Option<u32>, Option<String>) {
    let lower = output.to_lowercase();
    let fail_markers = [
        "error:",
        "failed",
        "failure",
        "exception",
        "panic:",
        "traceback",
        "error ts",
    ];
    let pass_markers = [
        "all checks passed",
        "success",
        "no issues found",
        "0 problems",
        "0 errors",
        "build succeeded",
        "compiled successfully",
    ];
    let evidence = |needle: &str| {
        output
            .lines()
            .rev()
            .find(|l| l.to_lowercase().contains(needle))
            .map(|l| l.trim().to_string())
    };
    for m in pass_markers {
        if lower.contains(m) {
            return (Outcome::Passed, None, None, evidence(m));
        }
    }
    for m in fail_markers {
        if lower.contains(m) {
            return (Outcome::Failed, None, None, evidence(m));
        }
    }
    // An empty output from a linter or build conventionally means "nothing to report".
    if output.trim().is_empty() && kind != CheckKind::Test {
        return (Outcome::Passed, None, None, None);
    }
    (Outcome::Unknown, None, None, None)
}

/// Extract every recognised check run from one Bash command and its output.
pub fn analyse(
    seq: usize,
    command: &str,
    output: &str,
    interrupted: bool,
    is_error: bool,
) -> Vec<CheckRun> {
    let mut runs = Vec::new();
    for segment in split_segments(command) {
        let Some((kind, runner)) = classify(&segment) else {
            continue;
        };
        let mut caveats = caveats_for(&segment, kind);
        caveats.extend(suppression_caveats(command));
        if kind == CheckKind::Test {
            if let Some(c) = targets_specific_tests(&segment) {
                if !caveats.iter().any(|x| matches!(x, Caveat::SubsetOnly(_))) {
                    caveats.push(c);
                }
            }
        }

        let (outcome, passed, failed, evidence_line) = if interrupted || is_error {
            (Outcome::DidNotComplete, None, None, None)
        } else {
            parse_outcome(&runner, kind, output)
        };
        if outcome == Outcome::Failed && failed == Some(0) && passed == Some(0) {
            caveats.push(Caveat::NoTestsRan);
        }

        runs.push(CheckRun {
            seq,
            kind,
            runner,
            command: segment,
            outcome,
            caveats,
            passed,
            failed,
            evidence_line,
        });
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(cmd: &str, out: &str) -> CheckRun {
        let mut r = analyse(1, cmd, out, false, false);
        assert_eq!(r.len(), 1, "expected exactly one run for `{cmd}`");
        r.remove(0)
    }

    #[test]
    fn pytest_pass_and_fail() {
        let ok = one("uv run pytest -q", "1098 passed, 181 warnings in 71.48s");
        assert_eq!(ok.outcome, Outcome::Passed);
        assert_eq!(ok.passed, Some(1098));
        assert!(ok.is_clean_pass());

        let bad = one("pytest", "3 passed, 2 failed in 1.2s");
        assert_eq!(bad.outcome, Outcome::Failed);
        assert_eq!(bad.failed, Some(2));
    }

    #[test]
    fn pytest_collecting_nothing_is_not_a_pass() {
        let r = one(
            "pytest tests/",
            "collected 0 items\n\nno tests ran in 0.01s",
        );
        assert_eq!(r.outcome, Outcome::Failed);
    }

    #[test]
    fn cargo_test_summary() {
        let ok = one(
            "cargo test",
            "test result: ok. 12 passed; 0 failed; 0 ignored",
        );
        assert_eq!(ok.outcome, Outcome::Passed);
        let bad = one(
            "cargo test",
            "test result: FAILED. 10 passed; 2 failed; 0 ignored",
        );
        assert_eq!(bad.outcome, Outcome::Failed);
        assert_eq!(bad.failed, Some(2));
    }

    #[test]
    fn jest_summary() {
        let bad = one("npx jest", "Tests:       1 failed, 4 passed, 5 total");
        assert_eq!(bad.outcome, Outcome::Failed);
        let ok = one("npx jest", "Tests:       5 passed, 5 total");
        assert_eq!(ok.outcome, Outcome::Passed);
    }

    #[test]
    fn go_test_fail_wins() {
        let bad = one("go test ./...", "ok  \tpkg/a\t0.1s\nFAIL\tpkg/b\t0.2s");
        assert_eq!(bad.outcome, Outcome::Failed);
    }

    #[test]
    fn mypy_and_ruff() {
        assert_eq!(
            one(
                "uv run mypy src",
                "Success: no issues found in 73 source files"
            )
            .outcome,
            Outcome::Passed
        );
        assert_eq!(
            one("uv run ruff check .", "All checks passed!").outcome,
            Outcome::Passed
        );
    }

    #[test]
    fn suppressed_failure_is_caveated() {
        let r = one("pytest -q || true", "1 passed");
        assert!(r
            .caveats
            .iter()
            .any(|c| matches!(c, Caveat::FailureSuppressed(_))));
        assert!(!r.is_clean_pass());
    }

    #[test]
    fn subset_runs_are_caveated() {
        let r = one("pytest -k test_login", "1 passed");
        assert!(r.caveats.iter().any(|c| matches!(c, Caveat::SubsetOnly(_))));

        let f = one("pytest tests/test_auth.py", "4 passed");
        assert!(f.caveats.iter().any(|c| matches!(c, Caveat::SubsetOnly(_))));
    }

    #[test]
    fn interrupted_run_did_not_complete() {
        let r = analyse(1, "pytest", "", true, false).remove(0);
        assert_eq!(r.outcome, Outcome::DidNotComplete);
    }

    #[test]
    fn compound_command_yields_multiple_runs() {
        let runs = analyse(
            1,
            "uv run ruff check . && uv run mypy src && uv run pytest -q",
            "All checks passed!\nSuccess: no issues found in 3 source files\n5 passed",
            false,
            false,
        );
        assert_eq!(runs.len(), 3);
        assert!(runs.iter().any(|r| r.kind == CheckKind::Test));
        assert!(runs.iter().any(|r| r.kind == CheckKind::TypeCheck));
        assert!(runs.iter().any(|r| r.kind == CheckKind::Lint));
    }

    #[test]
    fn quoted_separators_do_not_split() {
        let segs = split_segments("echo 'a && b' && pytest");
        assert_eq!(segs.len(), 2);
    }

    #[test]
    fn pipes_do_not_start_a_new_command() {
        // A single `|` is a pipe; only `||` separates commands.
        let segs = split_segments("pytest -q | tail -20");
        assert_eq!(segs.len(), 1);
    }

    #[test]
    fn handles_multibyte_characters() {
        // Regression: byte-indexing this input used to split a multi-byte character and panic.
        let cmd = "cat <<'EOF'\nPhase 5 — harness sprint, naïve approach 日本語\nEOF\npytest -q";
        let segs = split_segments(cmd);
        assert!(segs.iter().any(|s| s.contains("pytest")));

        let runs = analyse(1, cmd, "5 passed", false, false);
        assert_eq!(runs.len(), 1);
    }

    #[test]
    fn non_check_commands_are_ignored() {
        assert!(analyse(1, "ls -la", "a\nb", false, false).is_empty());
        assert!(analyse(1, "git status", "clean", false, false).is_empty());
    }

    #[test]
    fn shell_test_builtin_is_not_a_test_run() {
        // Regression: `test -f` was read as `npm test`, which would quietly satisfy a
        // "tests pass" claim in a session where no suite ever ran.
        assert!(analyse(1, "test -f /etc/hosts && echo yes", "yes", false, false).is_empty());
        assert!(analyse(1, "test -d build || mkdir build", "", false, false).is_empty());
        assert!(analyse(1, "[ -f x ] && test -x y", "", false, false).is_empty());
    }

    #[test]
    fn js_package_manager_test_scripts_are_recognised() {
        for cmd in [
            "npm test",
            "npm run test",
            "yarn test",
            "pnpm run test",
            "bun test",
        ] {
            let runs = analyse(1, cmd, "Tests:       5 passed, 5 total", false, false);
            assert_eq!(runs.len(), 1, "not recognised: {cmd}");
            assert_eq!(runs[0].kind, CheckKind::Test);
        }
    }

    #[test]
    fn recognises_runners_behind_a_path_or_interpreter() {
        // Regression: real sessions almost never call a runner by its bare name. Missing these
        // made genuine test runs invisible and honest claims look unsupported.
        for cmd in [
            ".venv-test/bin/python -m pytest tests/ -q",
            "/usr/local/bin/python3.12 -m pytest",
            "./node_modules/.bin/jest",
            ".venv/bin/pytest",
            "uvx pytest",
            "poetry run python -m pytest",
        ] {
            let runs = analyse(1, cmd, "5 passed", false, false);
            assert_eq!(runs.len(), 1, "not recognised: {cmd}");
            assert_eq!(runs[0].kind, CheckKind::Test, "wrong kind for: {cmd}");
        }
    }

    #[test]
    fn path_qualified_run_supports_a_claim_cleanly() {
        // A whole-suite run via a venv interpreter must count as a clean pass.
        let r = one(
            ".venv-test/bin/python -m pytest tests/ -q",
            "40 passed in 3.2s",
        );
        assert_eq!(r.outcome, Outcome::Passed);
        assert!(r.is_clean_pass(), "unexpected caveats: {:?}", r.caveats);
    }
}
