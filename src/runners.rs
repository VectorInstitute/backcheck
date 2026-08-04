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

static CLEAN_FAIL: OnceLock<Regex> = OnceLock::new();

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
    // Continuous integration is where a lot of verification actually happens. An agent that
    // waits on `gh pr checks` and reads the result has evidence, even though nothing ran locally.
    if lower.starts_with("gh pr checks") || lower.starts_with("gh run watch") {
        return Some((CheckKind::Test, "ci checks".into()));
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
    // pre-commit fans out to the project's own linters, so its summary stands in for them.
    if lower.starts_with("pre-commit run") {
        return Some((CheckKind::Lint, "pre-commit".into()));
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
    if lower.starts_with("mkdocs build") {
        return Some((CheckKind::Build, "mkdocs".into()));
    }
    if lower.starts_with("build")
        || lower.starts_with("hatch build")
        || lower.starts_with("maturin build")
    {
        // `python -m build` arrives here with the interpreter already stripped.
        return Some((CheckKind::Build, "python build".into()));
    }
    if lower.starts_with("docker build") || lower.starts_with("docker buildx build") {
        return Some((CheckKind::Build, "docker build".into()));
    }
    if lower.starts_with("sphinx-build") {
        return Some((CheckKind::Build, "sphinx".into()));
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

/// A subset the runner itself reported, read from its output rather than its arguments.
fn reported_subset(output: &str) -> Option<Caveat> {
    static FILTERED: OnceLock<Regex> = OnceLock::new();
    let n: u32 = re(&FILTERED, r"(\d+) filtered out")
        .captures(output)
        .and_then(|c| c[1].parse().ok())?;
    if n == 0 {
        return None;
    }
    Some(Caveat::SubsetOnly(format!("{n} tests filtered out")))
}

/// Does the command also hand the runner a whole directory to walk?
///
/// `pytest tests/test_orchestrator.py tests/` names one file and then the entire suite, so it
/// runs everything; treating it as a subset because a filename appears would be wrong.
fn covers_a_directory(segment: &str) -> bool {
    segment
        .split_whitespace()
        .skip(1)
        .filter(|t| !t.starts_with('-'))
        .any(|t| {
            let t = t.trim_matches(|c| c == '"' || c == '\'');
            t.ends_with('/') || t == "." || t == "./"
        })
}

/// Does the command name specific test files or test ids, rather than a whole suite?
fn targets_specific_tests(segment: &str) -> Option<Caveat> {
    if covers_a_directory(segment) {
        return None;
    }
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
            // `ruff check --fix` reports what it found and then what it repaired. "Found 2
            // errors (2 fixed, 0 remaining)" is a clean result, not a failing one.
            static FIXED: OnceLock<Regex> = OnceLock::new();
            if let Some(c) = find(r"\((\d+) fixed, (\d+) remaining\)", &FIXED) {
                let remaining: u32 = c[2].parse().unwrap_or(1);
                return if remaining == 0 {
                    (
                        Outcome::Passed,
                        None,
                        Some(0),
                        line_containing("remaining)"),
                    )
                } else {
                    (
                        Outcome::Failed,
                        None,
                        Some(remaining),
                        line_containing("remaining)"),
                    )
                };
            }
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
            // `ruff check -q` prints nothing when it is happy, so callers routinely append
            // their own marker (`&& echo RUFF CLEAN`). Let the generic reading find it.
            generic_outcome(output, kind)
        }
        "clippy" | "cargo check" | "cargo build" => {
            static E: OnceLock<Regex> = OnceLock::new();
            if let Some(c) = find(r"error(?:\[E\d+\])?:", &E) {
                let _ = c;
                return (Outcome::Failed, None, None, line_containing("error"));
            }
            if output.contains("Finished") || output.contains("Compiling") {
                // Match cargo's own progress line rather than the word "finished", which
                // also ends a test summary ("... finished in 0.79s") and would cite a test
                // result as proof that a build succeeded.
                let ev = output
                    .lines()
                    .rev()
                    .find(|l| {
                        let t = l.trim_start();
                        t.starts_with("Finished") || t.starts_with("Compiling")
                    })
                    .map(|l| l.trim().to_string());
                return (Outcome::Passed, None, Some(0), ev);
            }
            (Outcome::Unknown, None, None, None)
        }
        "npm build" => {
            // A JS build is a stack of tools, each with its own success line: vite's
            // `✓ built in 1.04s`, Next's route table, webpack's compile message. Failures
            // surface as TypeScript diagnostics or an explicit build error.
            static FAIL: OnceLock<Regex> = OnceLock::new();
            if re(
                &FAIL,
                r"(?i)error TS\d+|error during build|build failed|ERROR in |Failed to compile|✗ build",
            )
            .is_match(output)
            {
                return (
                    Outcome::Failed,
                    None,
                    None,
                    line_containing("error").or_else(|| line_containing("fail")),
                );
            }
            static OK: OnceLock<Regex> = OnceLock::new();
            if re(
                &OK,
                r"(?i)✓ built in|built in \d|compiled successfully|Compiled successfully|webpack compiled|✓ \d+ modules transformed|prerendered as static content|Build complete",
            )
            .is_match(output)
            {
                return (
                    Outcome::Passed,
                    None,
                    Some(0),
                    line_containing("built in")
                        .or_else(|| line_containing("compiled"))
                        .or_else(|| line_containing("static content")),
                );
            }
            generic_outcome(output, kind)
        }
        "ci checks" => {
            // `gh pr checks` prints one tab-separated row per check: name, state, duration, url.
            let mut passed = 0u32;
            let mut failed = 0u32;
            for line in output.lines() {
                let state = line.split('\t').nth(1).unwrap_or("").trim().to_lowercase();
                match state.as_str() {
                    "pass" | "success" | "skipping" | "skipped" | "neutral" => passed += 1,
                    "fail" | "failure" | "cancelled" | "timed_out" => failed += 1,
                    _ => {}
                }
            }
            let ev = output
                .lines()
                .rev()
                .find(|l| l.contains('\t'))
                .map(|l| l.split('\t').take(2).collect::<Vec<_>>().join(": "));
            match (passed, failed) {
                (_, f) if f > 0 => (Outcome::Failed, Some(passed), Some(f), ev),
                (p, _) if p > 0 => (Outcome::Passed, Some(p), Some(0), ev),
                _ => (Outcome::Unknown, None, None, None),
            }
        }
        "pre-commit" => {
            // Each hook reports "name.....Passed" or "Failed"; any failure fails the run.
            if output.contains("Failed") {
                return (Outcome::Failed, None, None, line_containing("failed"));
            }
            if output.contains("Passed") || output.contains("Skipped") {
                return (Outcome::Passed, None, Some(0), line_containing("passed"));
            }
            generic_outcome(output, kind)
        }
        "mkdocs" | "sphinx" => {
            if output.contains("Aborted") || output.to_lowercase().contains("error") {
                return (Outcome::Failed, None, None, line_containing("error"));
            }
            if output.contains("Documentation built in") || output.contains("build succeeded") {
                return (Outcome::Passed, None, Some(0), line_containing("built in"));
            }
            generic_outcome(output, kind)
        }
        "python build" => {
            if output.contains("Successfully built") {
                return (
                    Outcome::Passed,
                    None,
                    Some(0),
                    line_containing("successfully built"),
                );
            }
            generic_outcome(output, kind)
        }
        "docker build" => {
            static OK: OnceLock<Regex> = OnceLock::new();
            if re(
                &OK,
                r"(?i)naming to |writing image sha256|Successfully built|DONE \d",
            )
            .is_match(output)
            {
                return (Outcome::Passed, None, Some(0), line_containing("naming to"));
            }
            generic_outcome(output, kind)
        }
        "tsc" => {
            // TypeScript prints nothing when it is happy, and every diagnostic it does emit
            // carries a TS error code. That makes the absence of a code a reliable pass even
            // when the surrounding output belongs to another tool in the same chain.
            static TS: OnceLock<Regex> = OnceLock::new();
            if re(&TS, r"error TS\d+").is_match(output) {
                let n = re(&TS, r"error TS\d+").find_iter(output).count() as u32;
                return (Outcome::Failed, None, Some(n), line_containing("error ts"));
            }
            (Outcome::Passed, None, Some(0), None)
        }
        "pyright" | "eslint" | "flake8" | "pylint" | "golangci-lint" | "biome" | "format check"
        | "go build" => generic_outcome(output, kind),
        _ => generic_outcome(output, kind),
    }
}

/// Conservative fallback: only call it a pass or a fail on unambiguous signals.
fn generic_outcome(
    output: &str,
    kind: CheckKind,
) -> (Outcome, Option<u32>, Option<u32>, Option<String>) {
    let lower = output.to_lowercase();

    // A hand-written success marker (`&& echo "RUFF CLEAN"`) is only trustworthy for a
    // non-test check, where the tool itself is silent on success. A test runner that printed
    // nothing has not demonstrated anything.
    if kind != CheckKind::Test {
        static CLEAN: OnceLock<Regex> = OnceLock::new();
        if re(&CLEAN, r"(?i)\b(clean|ok|passed|no problems)\b").is_match(&lower)
            && !re(&CLEAN_FAIL, r"(?i)\b(error|failed|failure|traceback)\b").is_match(&lower)
        {
            let ev = output
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string());
            return (Outcome::Passed, None, Some(0), ev);
        }
    }
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
        "built in ",
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
/// The literal text an `echo` segment prints, when it is a plain constant.
///
/// Anything interpolated is unusable as a landmark, so `$VAR` and command substitution
/// disqualify the segment.
fn echo_literal(segment: &str) -> Option<String> {
    let rest = segment.trim().strip_prefix("echo ")?.trim();
    if rest.contains('$') || rest.contains('`') || rest.starts_with('-') {
        return None;
    }
    let lit = rest.trim_matches(|c| c == '"' || c == '\'').trim();
    if lit.len() < 3 {
        return None;
    }
    Some(lit.to_string())
}

/// Split a chained command's output into the region produced by each segment.
///
/// Agents habitually separate the parts of a chained command with `echo "=== lint ==="`,
/// which leaves a findable landmark in the combined output. Where those landmarks can be
/// located, each check is parsed against only the text it actually produced. Without this
/// every parser sees the whole stream, so a linter's "All checks passed!" can be reported as
/// the evidence for a claim about tests.
fn attribute_output<'a>(segments: &[String], output: &'a str) -> Vec<&'a str> {
    // Locate each echo landmark, scanning forward so repeated markers stay in order.
    let mut marks: Vec<Option<(usize, usize)>> = vec![None; segments.len()];
    let mut cursor = 0usize;
    for (i, seg) in segments.iter().enumerate() {
        if let Some(lit) = echo_literal(seg) {
            if let Some(rel) = output.get(cursor..).and_then(|t| t.find(&lit)) {
                let start = cursor + rel;
                marks[i] = Some((start, start + lit.len()));
                cursor = start + lit.len();
            }
        }
    }

    segments
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let start = marks[..i]
                .iter()
                .rev()
                .flatten()
                .next()
                .map(|(_, end)| *end)
                .unwrap_or(0);
            let end = marks[i + 1..]
                .iter()
                .flatten()
                .next()
                .map(|(begin, _)| *begin)
                .unwrap_or(output.len());
            if start >= end {
                ""
            } else {
                output.get(start..end).unwrap_or(output)
            }
        })
        .collect()
}

pub fn analyse(
    seq: usize,
    command: &str,
    output: &str,
    interrupted: bool,
    is_error: bool,
) -> Vec<CheckRun> {
    let mut runs = Vec::new();
    let segments = split_segments(command);
    let regions = attribute_output(&segments, output);

    for (idx, segment) in segments.iter().enumerate() {
        let segment = segment.clone();
        let region = regions.get(idx).copied().unwrap_or(output);
        let Some((kind, runner)) = classify(&segment) else {
            continue;
        };
        let mut caveats = caveats_for(&segment, kind);
        if kind == CheckKind::Test {
            if let Some(c) = targets_specific_tests(&segment) {
                if !caveats.iter().any(|x| matches!(x, Caveat::SubsetOnly(_))) {
                    caveats.push(c);
                }
            }
            // Some runners say outright how much of the suite they skipped. cargo prints
            // "1584 filtered out" when a name filter narrowed the run, which is far more
            // reliable than inferring a subset from the command line.
            if let Some(c) = reported_subset(region) {
                if !caveats.iter().any(|x| matches!(x, Caveat::SubsetOnly(_))) {
                    caveats.push(c);
                }
            }
        }

        // Parse first, then decide what an error flag means. A failure in one part of a chain
        // ("echo ===" tripping the shell) marks the whole call as errored, but a runner that
        // already printed "1465 passed" plainly finished. Only fall back to "did not complete"
        // when the output leaves the result genuinely unknown.
        let parsed = parse_outcome(&runner, kind, region);
        let (outcome, passed, failed, evidence_line) =
            if parsed.0 == Outcome::Unknown && (interrupted || is_error) {
                (Outcome::DidNotComplete, None, None, None)
            } else {
                parsed
            };
        if outcome == Outcome::Failed && failed == Some(0) && passed == Some(0) {
            caveats.push(Caveat::NoTestsRan);
        }

        // Caveats about *visibility* only bite when the verdict rested on not seeing a
        // failure. backcheck reads output rather than exit status, so once a runner has
        // printed a definitive summary ("40 passed", "✓ built in 1.10s"), a trailing
        // `|| true` or a `| head` did not hide the answer and saying so is noise.
        // Caveats about *scope* are different: a subset run really did test less.
        if evidence_line.is_none() {
            caveats.extend(suppression_caveats(command));
        } else {
            caveats.retain(|c| !matches!(c, Caveat::OutputFiltered(_)));
        }

        // `-x` and `--maxfail` only cut a run short once something fails. A run that passed
        // was never truncated by them, so the flag says nothing about coverage.
        if outcome == Outcome::Passed {
            caveats.retain(|c| !matches!(c, Caveat::StopsEarly(_)));
        }

        // A JS build script usually runs the type checker on the way through, and says so:
        // "> tsc -b && vite build". A build that got past that line is also evidence the types
        // are clean, which is what the agent means by "tsc and the build pass".
        if kind == CheckKind::Build && outcome == Outcome::Passed {
            static TSC_STEP: OnceLock<Regex> = OnceLock::new();
            if re(&TSC_STEP, r"(?m)^\s*>\s*[^\n]*\btsc\b").is_match(region) {
                runs.push(CheckRun {
                    seq,
                    kind: CheckKind::TypeCheck,
                    runner: "tsc".into(),
                    command: segment.clone(),
                    outcome: Outcome::Passed,
                    caveats: caveats.clone(),
                    passed: None,
                    failed: Some(0),
                    evidence_line: Some("the build ran `tsc` and completed".to_string()),
                });
            }
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
    fn silent_tsc_is_a_pass_and_ts_codes_are_a_failure() {
        // tsc says nothing on success, so "no output" is the result, not an unreadable one.
        assert_eq!(one("npx tsc --noEmit", "").outcome, Outcome::Passed);
        // Another tool's "error" in the same chain must not be read as a type error.
        let chained = analyse(
            1,
            "npx tsc -b && npx eslint src/",
            "1 error and 0 warnings",
            false,
            false,
        );
        let ts = chained
            .iter()
            .find(|r| r.runner == "tsc")
            .expect("tsc should be recognised");
        assert_eq!(ts.outcome, Outcome::Passed);
        let bad = one(
            "npx tsc -b",
            "src/pages/StatusPage.tsx(5,1): error TS6133: 'X' is declared but never read.",
        );
        assert_eq!(bad.outcome, Outcome::Failed);
    }

    #[test]
    fn ruff_fix_that_repaired_everything_is_a_pass() {
        // Regression: "Found 1 error (1 fixed, 0 remaining)" was read as a failing lint run
        // and contradicted an honest "lint passes".
        let fixed = one(
            "ruff check --fix src/",
            "Found 1 error (1 fixed, 0 remaining).",
        );
        assert_eq!(fixed.outcome, Outcome::Passed);

        let partial = one(
            "ruff check --fix src/",
            "Found 5 errors (2 fixed, 3 remaining).",
        );
        assert_eq!(partial.outcome, Outcome::Failed);
        assert_eq!(partial.failed, Some(3));
    }

    #[test]
    fn stops_early_flag_is_moot_when_nothing_failed() {
        // `pytest -x` that passed ran everything it selected; the flag never fired.
        let r = one("pytest -x", "74 passed in 0.07s");
        assert!(r.is_clean_pass(), "unexpected caveats: {:?}", r.caveats);
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
    fn suppressed_failure_is_caveated_only_when_it_could_hide_something() {
        // No readable summary: `|| true` really could be concealing the outcome.
        let hidden = one("pytest -q || true", "");
        assert!(hidden
            .caveats
            .iter()
            .any(|c| matches!(c, Caveat::FailureSuppressed(_))));

        // A definitive summary settles it, and the exit code was never consulted anyway.
        let shown = one("pytest -q || true", "1 passed in 0.2s");
        assert!(
            !shown
                .caveats
                .iter()
                .any(|c| matches!(c, Caveat::FailureSuppressed(_))),
            "an explicit pass line makes the suppression moot: {:?}",
            shown.caveats
        );
        assert!(shown.is_clean_pass());
    }

    #[test]
    fn filtering_is_moot_once_the_summary_survived_it() {
        // `| tail` and friends only matter if they removed the answer; here they did not.
        let r = one(
            "cargo test | head -20",
            "test result: ok. 65 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1s",
        );
        assert!(r.is_clean_pass(), "unexpected caveats: {:?}", r.caveats);
    }

    #[test]
    fn subset_runs_are_caveated() {
        let r = one("pytest -k test_login", "1 passed");
        assert!(r.caveats.iter().any(|c| matches!(c, Caveat::SubsetOnly(_))));

        let f = one("pytest tests/test_auth.py", "4 passed");
        assert!(f.caveats.iter().any(|c| matches!(c, Caveat::SubsetOnly(_))));
    }

    #[test]
    fn a_named_file_beside_a_whole_directory_is_not_a_subset() {
        // Regression: `pytest tests/test_orchestrator.py tests/` runs the entire suite, but the
        // filename made it look like only one file had been tested.
        let r = one(
            "uv run pytest tests/test_orchestrator.py tests/ -q",
            "152 passed in 0.87s",
        );
        assert!(
            r.is_clean_pass(),
            "the directory argument means everything ran: {:?}",
            r.caveats
        );

        let dot = one("pytest . -q", "40 passed");
        assert!(dot.is_clean_pass());
    }

    #[test]
    fn interrupted_run_did_not_complete() {
        let r = analyse(1, "pytest", "", true, false).remove(0);
        assert_eq!(r.outcome, Outcome::DidNotComplete);
    }

    #[test]
    fn a_later_broken_segment_does_not_erase_an_earlier_result() {
        // Regression: `echo ===` tripped the shell and flagged the whole call as errored, so a
        // pytest run that had already printed "1465 passed" was reported as never completing.
        let runs = analyse(
            1,
            "uv run pytest -q; echo ===; uv run ruff check .",
            "1465 passed, 173 warnings in 108.51s\n(eval):1: == not found",
            false,
            true,
        );
        let t = runs.iter().find(|r| r.kind == CheckKind::Test).unwrap();
        assert_eq!(t.outcome, Outcome::Passed);
        assert_eq!(t.passed, Some(1465));
    }

    #[test]
    fn a_build_that_ran_tsc_also_evidences_the_type_check() {
        // `npm run build` prints the script it is running; when that includes tsc, a completed
        // build is evidence the types are clean too.
        let runs = analyse(
            1,
            "npm run build",
            "> site@1.0.0 build\n> tsc -b && vite build\n\nvite v6.4.2 building for production...\n✓ built in 1.08s",
            false,
            false,
        );
        let tsc = runs.iter().find(|r| r.kind == CheckKind::TypeCheck);
        assert!(tsc.is_some(), "tsc step should be recorded: {runs:?}");
        assert_eq!(tsc.unwrap().outcome, Outcome::Passed);

        // A build with no type-checking step must not manufacture one.
        let plain = analyse(1, "npm run build", "✓ built in 1.08s", false, false);
        assert!(plain.iter().all(|r| r.kind != CheckKind::TypeCheck));
    }

    #[test]
    fn reads_ci_check_results() {
        // `gh pr checks` is how a lot of verification actually gets confirmed.
        let ok = one(
            "gh pr checks 261 --watch",
            "code-checks\tpass\t1m2s\thttps://x\nunit-tests\tpass\t26s\thttps://y",
        );
        assert_eq!(ok.outcome, Outcome::Passed);
        assert_eq!(ok.passed, Some(2));

        let bad = one(
            "gh pr checks 261",
            "code-checks\tpass\t1m2s\thttps://x\nunit-tests\tfail\t26s\thttps://y",
        );
        assert_eq!(bad.outcome, Outcome::Failed);
    }

    #[test]
    fn reads_pre_commit_results() {
        let ok = one(
            "uv run pre-commit run --all-files",
            "ruff.....................................................................Passed\nmypy.....................................................................Passed",
        );
        assert_eq!(ok.outcome, Outcome::Passed);

        let bad = one(
            "pre-commit run --all-files",
            "ruff.....................................................................Failed",
        );
        assert_eq!(bad.outcome, Outcome::Failed);
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
    fn a_filtered_cargo_run_is_not_a_whole_suite() {
        // `cargo test <name>` passing 2 of 1586 tests does not back "the tests pass".
        let r = one(
            "cargo test -p dynamo-llm --lib rank_reset",
            "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 1584 filtered out; finished in 0.01s",
        );
        assert_eq!(r.outcome, Outcome::Passed);
        assert!(
            r.caveats.iter().any(|c| matches!(c, Caveat::SubsetOnly(_))),
            "1584 filtered out must qualify the pass: {:?}",
            r.caveats
        );
        assert!(!r.is_clean_pass());

        // A full run reports nothing filtered and stays a clean pass.
        let full = one(
            "cargo test",
            "test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1s",
        );
        assert!(full.is_clean_pass());
    }

    #[test]
    fn build_evidence_is_not_a_test_summary() {
        // Regression: "finished in 0.79s" inside a test summary was cited as proof a build
        // succeeded, because the parser searched for the bare word "finished".
        let r = one(
            "cargo build",
            "test result: ok. 190 passed; 0 failed; finished in 0.79s\n    Finished `dev` profile [unoptimized] target(s) in 5.13s",
        );
        assert_eq!(r.outcome, Outcome::Passed);
        let ev = r.evidence_line.unwrap_or_default();
        assert!(ev.starts_with("Finished"), "cited the wrong line: {ev}");
    }

    #[test]
    fn each_check_is_parsed_against_its_own_output() {
        // Real shape from a session: pytest and ruff chained with an echo landmark between
        // them. Parsing both against the whole stream made ruff's "All checks passed!" the
        // evidence for the claim about tests.
        let runs = analyse(
            1,
            "pytest tests/ -q 2>&1 | tail -3; echo \"---RUFF---\"; ruff check src/ tests/",
            "40 passed in 1.30s\n---RUFF---\nAll checks passed!",
            false,
            false,
        );
        let test = runs.iter().find(|r| r.kind == CheckKind::Test).unwrap();
        assert_eq!(test.outcome, Outcome::Passed);
        assert_eq!(
            test.evidence_line.as_deref(),
            Some("40 passed in 1.30s"),
            "a claim about tests must cite the test runner, not the linter"
        );

        let lint = runs.iter().find(|r| r.kind == CheckKind::Lint).unwrap();
        assert_eq!(lint.outcome, Outcome::Passed);
        assert_eq!(lint.evidence_line.as_deref(), Some("All checks passed!"));
    }

    #[test]
    fn reads_a_hand_written_success_marker() {
        // `ruff check -q` is silent when it passes, so callers add their own marker.
        let runs = analyse(
            1,
            "pytest -q 2>&1 | tail -3; echo \"---RUFF---\"; ruff check src/ -q && echo \"RUFF CLEAN\"",
            "40 passed in 1.30s\n---RUFF---\nRUFF CLEAN",
            false,
            false,
        );
        let lint = runs.iter().find(|r| r.kind == CheckKind::Lint).unwrap();
        assert_eq!(lint.outcome, Outcome::Passed);
    }

    #[test]
    fn a_silent_marker_does_not_vouch_for_tests() {
        // The same trick must not work for a test runner: printing "CLEAN" proves nothing
        // about a suite that never reported running anything.
        let runs = analyse(1, "pytest -q && echo CLEAN", "CLEAN", false, false);
        assert_ne!(runs[0].outcome, Outcome::Passed);
    }

    #[test]
    fn reads_javascript_build_output() {
        // Real output from `npm run build` on a vite project. Reporting this as
        // "outcome cannot be read" told the user nothing they did not already know.
        let vite = one(
            "npm run build 2>&1 | tail -60",
            "dist/assets/index-CkLkKQeO.js  333.87 kB │ gzip: 108.17 kB\n\n✓ built in 568ms",
        );
        assert_eq!(vite.outcome, Outcome::Passed);

        let next = one(
            "npm run build",
            "○  (Static)   prerendered as static content\nƒ  (Dynamic)  server-rendered on demand",
        );
        assert_eq!(next.outcome, Outcome::Passed);

        // A TypeScript diagnostic means the build did not succeed.
        let broken = one(
            "npm run build",
            "> tsc -b && vite build\n\nsrc/pages/StatusPage.tsx(5,1): error TS6133: 'X' is declared but its value is never read.",
        );
        assert_eq!(broken.outcome, Outcome::Failed);
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
