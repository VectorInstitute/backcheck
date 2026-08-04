//! Matching claims against the evidence ledger.
//!
//! Each claim gets a verdict and, importantly, the reason for it. A verdict without its supporting
//! line of output is just another assertion the user has to take on faith.

use std::path::Path;

use crate::claims::{Claim, ClaimKind};
use crate::evidence::{GitOpKind, Ledger};
use crate::runners::{CheckKind, CheckRun, Outcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// Evidence in the transcript backs the claim.
    Supported,
    /// The claim could not be checked (no matching evidence recorded, and no way to check live).
    Inconclusive,
    /// Evidence exists but qualifies the claim: a stale run, a subset, a suppressed failure.
    Qualified,
    /// Nothing in the transcript supports the claim.
    Unsupported,
    /// The evidence says the opposite.
    Contradicted,
}

impl Verdict {
    pub fn is_problem(&self) -> bool {
        matches!(
            self,
            Verdict::Unsupported | Verdict::Contradicted | Verdict::Qualified
        )
    }

    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Supported => "supported",
            Verdict::Inconclusive => "inconclusive",
            Verdict::Qualified => "qualified",
            Verdict::Unsupported => "unsupported",
            Verdict::Contradicted => "contradicted",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Checked {
    pub claim: Claim,
    pub verdict: Verdict,
    /// Why this verdict, in one sentence.
    pub reason: String,
    /// The line of recorded output the verdict rests on.
    pub evidence: Option<String>,
}

/// Options affecting how far verification goes.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Consult the working tree and git repository, not just the transcript.
    pub live: bool,
    /// Directory to run live checks in.
    pub cwd: Option<String>,
}

pub fn verify(claims: &[Claim], ledger: &Ledger, opts: &Options) -> Vec<Checked> {
    claims
        .iter()
        .map(|c| match c.kind {
            ClaimKind::TestsPass => check_run_claim(c, ledger, CheckKind::Test),
            ClaimKind::TypeCheckPasses => check_run_claim(c, ledger, CheckKind::TypeCheck),
            ClaimKind::LintPasses => check_run_claim(c, ledger, CheckKind::Lint),
            ClaimKind::BuildPasses => check_run_claim(c, ledger, CheckKind::Build),
            ClaimKind::Committed => check_git_claim(c, ledger, GitOpKind::Commit, opts),
            ClaimKind::Pushed => check_git_claim(c, ledger, GitOpKind::Push, opts),
            ClaimKind::FileWritten => check_file_claim(c, ledger, opts),
        })
        .collect()
}

/// Verify a "the checks pass" claim against the runs recorded before it was made.
fn check_run_claim(claim: &Claim, ledger: &Ledger, kind: CheckKind) -> Checked {
    // Only evidence that predates the claim can support it.
    let prior: Vec<&CheckRun> = ledger
        .checks_of(kind)
        .into_iter()
        .filter(|r| r.seq < claim.seq)
        .collect();

    let mk = |verdict: Verdict, reason: String, evidence: Option<String>| Checked {
        claim: claim.clone(),
        verdict,
        reason,
        evidence,
    };

    let Some(last) = prior.last().copied() else {
        return mk(
            Verdict::Unsupported,
            format!(
                "no {} run appears anywhere in this session before the claim was made",
                kind.label()
            ),
            None,
        );
    };

    match last.outcome {
        Outcome::Failed => {
            let detail = match (last.failed, last.passed) {
                (Some(f), Some(p)) if f > 0 => format!("{f} failed, {p} passed"),
                (Some(f), None) if f > 0 => format!("{f} failed"),
                _ => "the run reported failures".to_string(),
            };
            mk(
                Verdict::Contradicted,
                format!("the last `{}` run {detail}", last.runner),
                last.evidence_line.clone(),
            )
        }
        Outcome::DidNotComplete => mk(
            Verdict::Unsupported,
            format!(
                "the last `{}` run never completed (interrupted or blocked)",
                last.runner
            ),
            last.evidence_line.clone(),
        ),
        Outcome::Unknown => mk(
            Verdict::Inconclusive,
            format!(
                "`{}` ran, but its output does not state an outcome backcheck can read",
                last.runner
            ),
            last.evidence_line.clone(),
        ),
        Outcome::Passed => {
            if !last.caveats.is_empty() {
                let caveats = last
                    .caveats
                    .iter()
                    .map(|c| c.describe())
                    .collect::<Vec<_>>()
                    .join("; ");
                return mk(
                    Verdict::Qualified,
                    format!("`{}` passed, but {caveats}", last.runner),
                    last.evidence_line.clone(),
                );
            }

            // A pass that predates the newest source edit says nothing about the current code.
            let later_writes = ledger.writes_after(last.seq);
            let source_writes: Vec<_> = later_writes
                .iter()
                .filter(|w| !w.is_test_file() && w.seq < claim.seq)
                .collect();
            if !source_writes.is_empty() {
                let files: Vec<String> = source_writes
                    .iter()
                    .map(|w| short_path(&w.path))
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let shown = files.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
                let more = files.len().saturating_sub(3);
                return mk(
                    Verdict::Qualified,
                    format!(
                        "`{}` passed, but {} file{} changed afterwards and were never re-tested ({}{})",
                        last.runner,
                        files.len(),
                        if files.len() == 1 { "" } else { "s" },
                        shown,
                        if more > 0 { format!(", +{more} more") } else { String::new() }
                    ),
                    last.evidence_line.clone(),
                );
            }

            mk(
                Verdict::Supported,
                format!("`{}` ran and passed with no edits after it", last.runner),
                last.evidence_line.clone(),
            )
        }
    }
}

/// Verify a commit or push claim, optionally consulting the repository itself.
fn check_git_claim(claim: &Claim, ledger: &Ledger, kind: GitOpKind, opts: &Options) -> Checked {
    let noun = match kind {
        GitOpKind::Commit => "commit",
        GitOpKind::Push => "push",
    };
    let prior = ledger
        .git_ops
        .iter()
        .rfind(|g| g.kind == kind && g.seq < claim.seq);

    let mk = |verdict: Verdict, reason: String, evidence: Option<String>| Checked {
        claim: claim.clone(),
        verdict,
        reason,
        evidence,
    };

    match prior {
        Some(op) if op.succeeded => mk(
            Verdict::Supported,
            format!("a `git {noun}` ran and reported success"),
            op.output_line.clone(),
        ),
        Some(op) => mk(
            Verdict::Contradicted,
            format!("`git {noun}` ran but did not succeed"),
            op.output_line.clone(),
        ),
        None => {
            // Nothing in the transcript. The repository may still show it, if the agent used a
            // tool backcheck cannot see.
            if opts.live && kind == GitOpKind::Commit {
                if let Some(dirty) = working_tree_dirty(opts.cwd.as_deref()) {
                    if dirty {
                        return mk(
                            Verdict::Unsupported,
                            format!("no `git {noun}` in this session, and the working tree still has uncommitted changes"),
                            Some("git status --porcelain reports modified files".into()),
                        );
                    }
                }
            }
            mk(
                Verdict::Unsupported,
                format!("no `git {noun}` appears in this session"),
                None,
            )
        }
    }
}

/// Verify that a file the agent said it wrote exists and was actually written.
fn check_file_claim(claim: &Claim, ledger: &Ledger, opts: &Options) -> Checked {
    let Some(subject) = claim.subject.as_deref() else {
        return Checked {
            claim: claim.clone(),
            verdict: Verdict::Inconclusive,
            reason: "no file path could be read from the claim".into(),
            evidence: None,
        };
    };

    let written = ledger
        .writes
        .iter()
        .any(|w| w.succeeded && w.seq < claim.seq && paths_match(&w.path, subject));
    if written {
        return Checked {
            claim: claim.clone(),
            verdict: Verdict::Supported,
            reason: format!("`{subject}` was written by a tool call in this session"),
            evidence: None,
        };
    }

    // The agent may have created it via a shell heredoc, which is legitimate but invisible to the
    // write ledger; fall back to the filesystem before calling the claim unsupported.
    if opts.live {
        if let Some(dir) = opts.cwd.as_deref() {
            let candidate = Path::new(dir).join(subject);
            if candidate.exists() {
                return Checked {
                    claim: claim.clone(),
                    verdict: Verdict::Supported,
                    reason: format!("`{subject}` exists on disk"),
                    evidence: None,
                };
            }
            return Checked {
                claim: claim.clone(),
                verdict: Verdict::Unsupported,
                reason: format!(
                    "`{subject}` was never written in this session and does not exist on disk"
                ),
                evidence: None,
            };
        }
    }

    Checked {
        claim: claim.clone(),
        verdict: Verdict::Inconclusive,
        reason: format!(
            "no write to `{subject}` recorded; run with --live to check the filesystem"
        ),
        evidence: None,
    }
}

/// Claims name files loosely ("parser.rs"), while the ledger holds absolute paths.
fn paths_match(recorded: &str, claimed: &str) -> bool {
    let r = recorded.replace('\\', "/");
    let c = claimed.replace('\\', "/");
    r == c || r.ends_with(&format!("/{c}")) || c.ends_with(&r) || {
        let rf = r.rsplit('/').next().unwrap_or(&r);
        let cf = c.rsplit('/').next().unwrap_or(&c);
        // Match on filename only when the claim gave no directory at all.
        !c.contains('/') && rf == cf
    }
}

fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.trim_end_matches('/').split('/').collect();
    if parts.len() <= 2 {
        return p.to_string();
    }
    parts[parts.len() - 2..].join("/")
}

fn working_tree_dirty(cwd: Option<&str>) -> Option<bool> {
    let dir = cwd?;
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(!String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims;
    use crate::transcript::Transcript;

    fn run(lines: &[serde_json::Value]) -> Vec<Checked> {
        let raw = lines
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let t = Transcript::parse_str(&raw);
        let l = Ledger::build(&t);
        let c = claims::extract(&t);
        verify(&c, &l, &Options::default())
    }

    fn bash(id: &str, cmd: &str) -> serde_json::Value {
        serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":id,"name":"Bash","input":{"command":cmd}}]}})
    }
    fn res(id: &str, out: &str) -> serde_json::Value {
        serde_json::json!({"type":"user","toolUseResult":{"stdout":out,"stderr":"","interrupted":false},
            "message":{"content":[{"type":"tool_result","tool_use_id":id,"content":out}]}})
    }
    fn say(text: &str) -> serde_json::Value {
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}})
    }
    fn edit(id: &str, path: &str) -> serde_json::Value {
        serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":id,"name":"Edit","input":{"file_path":path,"old_string":"a","new_string":"b"}}]}})
    }

    #[test]
    fn passing_run_supports_claim() {
        let v = run(&[
            bash("1", "pytest -q"),
            res("1", "12 passed"),
            say("All tests pass."),
        ]);
        assert_eq!(v[0].verdict, Verdict::Supported);
    }

    #[test]
    fn claim_without_any_run_is_unsupported() {
        let v = run(&[say("All tests pass.")]);
        assert_eq!(v[0].verdict, Verdict::Unsupported);
        assert!(v[0].reason.contains("no test run"));
    }

    #[test]
    fn failing_run_contradicts_claim() {
        let v = run(&[
            bash("1", "pytest -q"),
            res("1", "10 passed, 2 failed"),
            say("All tests pass."),
        ]);
        assert_eq!(v[0].verdict, Verdict::Contradicted);
    }

    #[test]
    fn edits_after_the_run_qualify_the_claim() {
        let v = run(&[
            bash("1", "pytest -q"),
            res("1", "12 passed"),
            edit("2", "/repo/src/main.py"),
            res("2", "ok"),
            say("All tests pass."),
        ]);
        assert_eq!(v[0].verdict, Verdict::Qualified);
        assert!(v[0].reason.contains("changed afterwards"));
    }

    #[test]
    fn test_file_edits_do_not_trigger_staleness() {
        let v = run(&[
            bash("1", "pytest -q"),
            res("1", "12 passed"),
            edit("2", "/repo/tests/test_x.py"),
            res("2", "ok"),
            say("All tests pass."),
        ]);
        assert_eq!(v[0].verdict, Verdict::Supported);
    }

    #[test]
    fn subset_run_qualifies_a_blanket_claim() {
        let v = run(&[
            bash("1", "pytest -k test_login"),
            res("1", "1 passed"),
            say("All tests pass."),
        ]);
        assert_eq!(v[0].verdict, Verdict::Qualified);
    }

    #[test]
    fn run_after_the_claim_does_not_count() {
        let v = run(&[
            say("All tests pass."),
            bash("1", "pytest"),
            res("1", "5 passed"),
        ]);
        assert_eq!(v[0].verdict, Verdict::Unsupported);
    }

    #[test]
    fn commit_claim_checks_git_evidence() {
        let ok = run(&[
            bash("1", "git commit -m 'x'"),
            res("1", "[main abc] x"),
            say("I've committed the changes."),
        ]);
        assert_eq!(ok[0].verdict, Verdict::Supported);

        let bad = run(&[say("I've committed the changes.")]);
        assert_eq!(bad[0].verdict, Verdict::Unsupported);
    }

    #[test]
    fn interrupted_run_does_not_support_claim() {
        let raw = format!(
            "{}\n{}\n{}",
            bash("1", "pytest"),
            serde_json::json!({"type":"user","toolUseResult":{"stdout":"","stderr":"","interrupted":true},
                "message":{"content":[{"type":"tool_result","tool_use_id":"1","content":""}]}}),
            say("All tests pass.")
        );
        let t = Transcript::parse_str(&raw);
        let l = Ledger::build(&t);
        let v = verify(&claims::extract(&t), &l, &Options::default());
        assert_eq!(v[0].verdict, Verdict::Unsupported);
    }

    #[test]
    fn matches_loose_file_paths() {
        assert!(paths_match("/repo/src/parser.rs", "src/parser.rs"));
        assert!(paths_match("/repo/src/parser.rs", "parser.rs"));
        assert!(!paths_match("/repo/src/parser.rs", "src/other.rs"));
    }
}
