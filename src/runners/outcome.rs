//! Reading a runner's verdict out of what it printed.
//!
//! Transcripts carry no exit codes, so each supported tool needs its own summary line
//! understood. Every parser here returns [`Outcome::Unknown`] rather than guessing: a wrong
//! "verified" is far more damaging than an honest "could not tell".

use regex::Regex;
use std::sync::OnceLock;

use super::{re, CheckKind, Outcome, CLEAN_FAIL};

/// Read a runner's outcome out of its output.
pub(crate) fn parse_outcome(
    runner: &str,
    kind: CheckKind,
    output: &str,
    exclusive: bool,
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
            generic_outcome(output, kind, exclusive)
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
            generic_outcome(output, kind, exclusive)
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
        "ci run" => {
            // Only the newest run is about the work in hand, and `gh` prints it first. Older
            // rows are history, so aggregating them would let last week's failure contradict
            // today's claim.
            let row = output
                .lines()
                .map(str::trim)
                .find(|l| l.contains('\t') || l.contains("completed") || l.contains("in_progress"));
            let Some(row) = row else {
                return generic_outcome(output, kind, exclusive);
            };
            let lower = row.to_lowercase();
            if lower.contains("failure")
                || lower.contains("cancelled")
                || lower.contains("timed_out")
            {
                return (Outcome::Failed, None, Some(1), Some(row.to_string()));
            }
            if lower.contains("success") {
                return (Outcome::Passed, Some(1), Some(0), Some(row.to_string()));
            }
            // Queued or still running proves nothing yet.
            (Outcome::Unknown, None, None, None)
        }
        "import-linter" => {
            // "Contracts: 1 kept, 0 broken." names both counts, so the word "broken" is
            // present on a clean run too. Read the number rather than the word.
            static CONTRACTS: OnceLock<Regex> = OnceLock::new();
            if let Some(c) = find(r"Contracts:\s*(\d+) kept,\s*(\d+) broken", &CONTRACTS) {
                let broken: u32 = c[2].parse().unwrap_or(0);
                let ev = line_containing("contracts:");
                return if broken == 0 {
                    (Outcome::Passed, None, Some(0), ev)
                } else {
                    (Outcome::Failed, None, Some(broken), ev)
                };
            }
            generic_outcome(output, kind, exclusive)
        }
        "pre-commit" => {
            // Each hook reports "name.....Passed" or "Failed"; any failure fails the run.
            if output.contains("Failed") {
                return (Outcome::Failed, None, None, line_containing("failed"));
            }
            if output.contains("Passed") || output.contains("Skipped") {
                return (Outcome::Passed, None, Some(0), line_containing("passed"));
            }
            generic_outcome(output, kind, exclusive)
        }
        "mkdocs" | "sphinx" => {
            if output.contains("Aborted") || output.to_lowercase().contains("error") {
                return (Outcome::Failed, None, None, line_containing("error"));
            }
            if output.contains("Documentation built in") || output.contains("build succeeded") {
                return (Outcome::Passed, None, Some(0), line_containing("built in"));
            }
            generic_outcome(output, kind, exclusive)
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
            generic_outcome(output, kind, exclusive)
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
            generic_outcome(output, kind, exclusive)
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
        | "go build" => generic_outcome(output, kind, exclusive),
        _ => generic_outcome(output, kind, exclusive),
    }
}

/// Conservative fallback: only call it a pass or a fail on unambiguous signals.
///
/// `exclusive` says whether this text is known to have come from this command alone. When a
/// chain like `pytest && ruff check .` cannot be split, every parser sees the same stream, and
/// a weak signal there belongs to whichever tool actually printed it. Reading pytest's
/// "1172 passed" as proof that ruff is happy is exactly the mistake this guards against.
fn generic_outcome(
    output: &str,
    kind: CheckKind,
    exclusive: bool,
) -> (Outcome, Option<u32>, Option<u32>, Option<String>) {
    let lower = output.to_lowercase();

    // A hand-written success marker (`&& echo "RUFF CLEAN"`) is only trustworthy for a
    // non-test check, where the tool itself is silent on success, and only when the text is
    // known to be this command's own.
    if kind != CheckKind::Test && exclusive {
        static CLEAN: OnceLock<Regex> = OnceLock::new();
        if re(&CLEAN, r"(?i)\b(clean|no problems|no issues|all good)\b").is_match(&lower)
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
    // Success phrasings, split by the kind of check they can legitimately vouch for. A build
    // finishing says nothing about a linter, so "built in" must not satisfy a lint check.
    let shared: &[&str] = &[
        "all checks passed",
        "no issues found",
        "0 problems",
        "0 errors",
    ];
    let specific: &[&str] = match kind {
        CheckKind::Build => &[
            "build succeeded",
            "compiled successfully",
            "built in ",
            "success",
        ],
        CheckKind::Test => &[],
        _ => &["success"],
    };
    let pass_markers: Vec<&str> = shared.iter().chain(specific.iter()).copied().collect();
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
