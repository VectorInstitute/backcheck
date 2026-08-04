//! Deciding what kind of check a command performs, and what qualifies its result.
//!
//! Adding a runner starts here: recognise the command, then teach [`super::outcome`] to read
//! what it prints.

use regex::Regex;
use std::sync::OnceLock;

use super::command::strip_invocation;
use super::{re, Caveat, CheckKind};

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
    // `gh run list` shows one row per workflow run, newest first, so only the first row is
    // about the work in hand. `gh run view` shows a single run. They are kept apart from
    // `gh pr checks`, where every row belongs to the same pull request.
    if lower.starts_with("gh run list") {
        return Some((CheckKind::Test, "ci run".into()));
    }
    if lower.starts_with("gh run view") {
        return Some((CheckKind::Test, "ci run".into()));
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
    if lower.starts_with("lint-imports") || lower.starts_with("import-linter") {
        return Some((CheckKind::Lint, "import-linter".into()));
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
pub(crate) fn suppression_caveats(full_command: &str) -> Vec<Caveat> {
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
pub(crate) fn caveats_for(segment: &str, kind: CheckKind) -> Vec<Caveat> {
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
pub(crate) fn reported_subset(output: &str) -> Option<Caveat> {
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
pub(crate) fn targets_specific_tests(segment: &str) -> Option<Caveat> {
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
