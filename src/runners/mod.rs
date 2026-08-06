//! Recognising verification commands and reading their outcome from their output.
//!
//! The work splits three ways, and a new runner usually touches only the first two:
//!
//! - [`classify`] decides what kind of check a command performs
//! - [`outcome`] reads the verdict out of what that command printed
//! - [`command`] handles shell mechanics: chaining, wrappers, and which output belongs to which step
//!
//! [`analyse`] joins them up for one Bash call from a transcript.

mod classify;
mod command;
mod outcome;

use regex::Regex;
use std::sync::OnceLock;

pub use classify::classify;
pub use command::split_segments;

use classify::{caveats_for, reported_subset, suppression_caveats, targets_specific_tests};
use command::{attribute_output, split_with_joins, Join, Region};
use outcome::parse_outcome;

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

pub fn analyse(
    seq: usize,
    command: &str,
    output: &str,
    interrupted: bool,
    is_error: bool,
) -> Vec<CheckRun> {
    let mut runs = Vec::new();
    let joined = split_with_joins(command);
    let segments: Vec<String> = joined.iter().map(|(s, _)| s.clone()).collect();
    let regions = attribute_output(&segments, output);
    // Which segment each run came from, so the `&&` inference below can find its neighbours.
    let mut origin: Vec<usize> = Vec::new();

    for (idx, segment) in segments.iter().enumerate() {
        let segment = segment.clone();
        let region = regions.get(idx).copied().unwrap_or(Region {
            text: output,
            exclusive: false,
        });
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
            if let Some(c) = reported_subset(region.text) {
                if !caveats.iter().any(|x| matches!(x, Caveat::SubsetOnly(_))) {
                    caveats.push(c);
                }
            }
        }

        // Parse first, then decide what an error flag means. A failure in one part of a chain
        // ("echo ===" tripping the shell) marks the whole call as errored, but a runner that
        // already printed "1465 passed" plainly finished. Only fall back to "did not complete"
        // when the output leaves the result genuinely unknown.
        let parsed = parse_outcome(&runner, kind, region.text, region.exclusive);
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
            if re(&TSC_STEP, r"(?m)^\s*>\s*[^\n]*\btsc\b").is_match(region.text) {
                origin.push(idx);
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

        origin.push(idx);
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

    infer_from_short_circuit(&mut runs, &origin, &joined, output);
    runs
}

/// Resolve unreadable results using what `&&` guarantees.
///
/// `ruff check . && mypy src` only reaches mypy if ruff exited zero. So when a later command in
/// an `&&` chain demonstrably ran, every command before it in that chain succeeded, whatever
/// its own output did or did not say. This is the one place backcheck can be certain about a
/// tool that printed nothing at all.
fn infer_from_short_circuit(
    runs: &mut [CheckRun],
    origin: &[usize],
    joined: &[(String, Join)],
    output: &str,
) {
    if output.trim().is_empty() {
        return;
    }
    // The furthest segment known to have produced something.
    let reached = runs
        .iter()
        .enumerate()
        .filter(|(i, r)| {
            matches!(r.outcome, Outcome::Passed | Outcome::Failed) && origin.get(*i).is_some()
        })
        .filter_map(|(i, _)| origin.get(i).copied())
        .max();
    let Some(reached) = reached else { return };

    for (i, run) in runs.iter_mut().enumerate() {
        let Some(&at) = origin.get(i) else { continue };
        if run.outcome != Outcome::Unknown || at >= reached {
            continue;
        }
        // Every join between this segment and the one that ran must be `&&` for the
        // guarantee to hold; a `;` or `||` in between breaks the chain of implication.
        if joined[at..reached].iter().all(|(_, j)| *j == Join::AndThen) {
            run.outcome = Outcome::Passed;
            run.failed = Some(0);
            run.evidence_line = Some(format!(
                "a later step in the same `&&` chain ran, so `{}` exited cleanly",
                run.runner
            ));
        }
    }
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
    fn shellcheck_pass_fail_and_ambiguous_output() {
        let ok = one("shellcheck scripts/check.sh", "");
        assert_eq!(ok.kind, CheckKind::Lint);
        assert_eq!(ok.runner, "shellcheck");
        assert_eq!(ok.outcome, Outcome::Passed);

        let bad = one(
            "shellcheck scripts/check.sh",
            "In scripts/check.sh line 3:\n\
echo $name\n\
     ^---^ SC2086 (info): Double quote to prevent globbing and word splitting.\n\
\n\
For more information:\n\
  https://www.shellcheck.net/wiki/SC2086 -- Double quote to prevent globbing and word splitting.",
        );
        assert_eq!(bad.outcome, Outcome::Failed);
        let evidence = bad.evidence_line.as_deref().unwrap_or_default();
        assert!(
            evidence.contains("SC2086") && !evidence.contains("shellcheck.net"),
            "evidence should quote the finding, not the wiki URL: {evidence}"
        );
        assert_eq!(bad.failed, Some(1));

        let ambiguous = one("shellcheck scripts/check.sh", "Checking scripts/check.sh");
        assert_eq!(ambiguous.outcome, Outcome::Unknown);
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
    fn import_linter_counts_broken_contracts() {
        // Regression: a clean run says "1 kept, 0 broken", and matching on the word "broken"
        // turned that into a failure that contradicted an honest claim.
        let ok = one(
            "uv run lint-imports",
            "Layered architecture KEPT\n\nContracts: 1 kept, 0 broken.",
        );
        assert_eq!(ok.outcome, Outcome::Passed);

        let bad = one(
            "uv run lint-imports",
            "Layered architecture BROKEN\n\nContracts: 0 kept, 1 broken.",
        );
        assert_eq!(bad.outcome, Outcome::Failed);
        assert_eq!(bad.failed, Some(1));
    }

    #[test]
    fn strips_a_timeout_wrapper() {
        // Regression: long suites are routinely wrapped, and the wrapper hid the run.
        for cmd in [
            "timeout 240 uv run pytest -q",
            "timeout 590 .venv/bin/python -m pytest",
            "timeout --signal=KILL 30 cargo test",
        ] {
            let runs = analyse(1, cmd, "12 passed in 1s", false, false);
            assert_eq!(runs.len(), 1, "not recognised: {cmd}");
            assert_eq!(runs[0].kind, CheckKind::Test, "wrong kind for {cmd}");
        }
    }

    #[test]
    fn reads_the_newest_workflow_run_only() {
        // `gh run list` is history, newest first. Older rows must not speak for today.
        let ok = one(
            "gh run list --branch main --limit 5",
            "completed\tsuccess\tfix: thing\tCI\tmain\tpush\t309\t45s\t1m\n\
             completed\tfailure\told break\tCI\tmain\tpush\t308\t50s\t2d",
        );
        assert_eq!(ok.outcome, Outcome::Passed, "the newest row succeeded");

        let bad = one(
            "gh run list",
            "completed\tfailure\tbroke it\tCI\tmain\tpush\t310\t20s\t1m\n\
             completed\tsuccess\tearlier\tCI\tmain\tpush\t309\t45s\t1d",
        );
        assert_eq!(bad.outcome, Outcome::Failed);

        // Still running is not yet evidence of anything.
        let pending = one(
            "gh run list",
            "in_progress\t\tdeploy\tCI\tmain\tpush\t311\t5s\t5s",
        );
        assert_eq!(pending.outcome, Outcome::Unknown);
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
    fn and_then_proves_the_earlier_command_succeeded() {
        // `ruff && mypy` only reaches mypy if ruff exited zero, so mypy's summary settles
        // ruff's silence. This is the one inference available for a tool that prints nothing.
        let runs = analyse(
            1,
            "uv run ruff check . && uv run mypy src",
            "Success: no issues found in 92 source files",
            false,
            false,
        );
        let lint = runs.iter().find(|r| r.runner == "ruff").unwrap();
        assert_eq!(lint.outcome, Outcome::Passed);
        assert!(lint
            .evidence_line
            .as_deref()
            .unwrap_or_default()
            .contains("&&"));
    }

    #[test]
    fn a_broken_chain_proves_nothing() {
        // With `;` the second command runs whatever the first did, so silence stays unread.
        let runs = analyse(
            1,
            "uv run ruff check . ; uv run mypy src",
            "Success: no issues found in 92 source files",
            false,
            false,
        );
        let lint = runs.iter().find(|r| r.runner == "ruff").unwrap();
        assert_eq!(lint.outcome, Outcome::Unknown);

        // `||` means the opposite: mypy running would imply ruff had failed.
        let alt = analyse(
            1,
            "uv run ruff check . || uv run mypy src",
            "Success: no issues found in 92 source files",
            false,
            false,
        );
        let l2 = alt.iter().find(|r| r.runner == "ruff").unwrap();
        assert_eq!(l2.outcome, Outcome::Unknown);
    }

    #[test]
    fn one_tool_never_vouches_for_another() {
        // Regression, and the worst kind: ruff printed nothing, pytest printed "1172 passed",
        // and the shared stream made backcheck report "lint passes: supported".
        let runs = analyse(
            1,
            "uv run pytest -q && uv run ruff check .",
            "1172 passed, 163 warnings in 41.54s",
            false,
            false,
        );
        let test = runs.iter().find(|r| r.kind == CheckKind::Test).unwrap();
        assert_eq!(test.outcome, Outcome::Passed);

        let lint = runs.iter().find(|r| r.kind == CheckKind::Lint).unwrap();
        assert_eq!(
            lint.outcome,
            Outcome::Unknown,
            "ruff said nothing; the test summary is not its result"
        );

        // A build finishing is likewise no evidence about a linter.
        let mixed = analyse(
            1,
            "npm run build && npx eslint src/",
            "✓ built in 566ms",
            false,
            false,
        );
        let el = mixed.iter().find(|r| r.runner == "eslint").unwrap();
        assert_eq!(el.outcome, Outcome::Unknown);
    }

    #[test]
    fn a_single_command_still_owns_its_output() {
        // The guard must not make ordinary single-step runs unreadable.
        assert_eq!(
            one("ruff check . -q && echo CLEAN", "CLEAN").outcome,
            Outcome::Passed
        );
        assert_eq!(one("npx tsc --noEmit", "").outcome, Outcome::Passed);
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
    fn js_lint_and_typecheck_scripts_are_recognised() {
        // `npm run lint` is about as common as `npm run build`; missing it left honest claims
        // about a clean linter reported as unsupported.
        let lint = one("npm run lint", "\n> eslint .\n");
        assert_eq!(lint.kind, CheckKind::Lint);

        let failing = one("npm run lint", "✖ 3 problems (3 errors, 0 warnings)");
        assert_eq!(failing.outcome, Outcome::Failed);
        assert_eq!(failing.failed, Some(3));

        assert_eq!(one("npm run typecheck", "").kind, CheckKind::TypeCheck);
    }

    #[test]
    fn a_runner_inside_a_shell_loop_is_still_a_runner() {
        // `for i in 1 2 3; do cargo test; done` splits to a segment beginning "do ".
        let runs = analyse(
            1,
            "for i in 1 2 3; do cargo test --lib; done",
            "test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
            false,
            false,
        );
        assert!(
            runs.iter().any(|r| r.kind == CheckKind::Test),
            "the loop body should still count: {runs:?}"
        );
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
