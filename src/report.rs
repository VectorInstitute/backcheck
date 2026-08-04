//! Rendering results for humans and for machines.

use serde::Serialize;

use crate::tamper::{Finding, Severity};
use crate::verify::{Checked, Verdict};

/// One check backcheck recognised while reading the session, for `--explain`.
pub struct SeenRun {
    pub kind: &'static str,
    pub runner: String,
    pub outcome: &'static str,
    pub detail: Option<String>,
}

/// Everything one run of backcheck concluded.
pub struct Report {
    pub session_id: Option<String>,
    pub transcript: String,
    pub checked: Vec<Checked>,
    pub findings: Vec<Finding>,
    /// Number of check runs seen, for the "nothing was ever run" case.
    pub runs_seen: usize,
    /// What was recognised, and what looked like a check but was not.
    pub seen: Vec<SeenRun>,
    pub unrecognised: Vec<String>,
}

impl Report {
    /// Claims whose verdict warrants the user's attention.
    pub fn problems(&self) -> Vec<&Checked> {
        self.checked
            .iter()
            .filter(|c| c.verdict.is_problem())
            .collect()
    }

    pub fn warnings(&self) -> Vec<&Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Warning)
            .collect()
    }

    /// True when there is something worth blocking or failing on.
    pub fn has_problems(&self) -> bool {
        !self.problems().is_empty() || !self.warnings().is_empty()
    }

    /// A one-line summary suitable for a status line or a hook message.
    pub fn headline(&self) -> String {
        let p = self.problems().len();
        let w = self.warnings().len();
        let claims = |n: usize| {
            format!(
                "{n} claim{} not fully supported",
                if n == 1 { "" } else { "s" }
            )
        };
        let weakened = |n: usize| {
            format!(
                "{n} sign{} the test suite was weakened",
                if n == 1 { "" } else { "s" }
            )
        };
        // Inconclusive is not a problem, but it is not a pass either. Folding it into
        // "all supported" would overstate what was actually verified.
        let unverified = self
            .checked
            .iter()
            .filter(|c| c.verdict == Verdict::Inconclusive)
            .count();
        let supported = self.checked.len() - unverified;

        match (p, w) {
            (0, 0) if self.checked.is_empty() => "no verifiable claims were made".to_string(),
            (0, 0) if unverified > 0 => format!(
                "{supported} claim{} supported, {unverified} could not be checked",
                if supported == 1 { "" } else { "s" }
            ),
            (0, 0) => format!(
                "{} claim{} checked, all supported",
                self.checked.len(),
                if self.checked.len() == 1 { "" } else { "s" }
            ),
            (p, 0) => claims(p),
            (0, w) => weakened(w),
            (p, w) => format!("{}, {}", claims(p), weakened(w)),
        }
    }
}

// ---------------------------------------------------------------- JSON

#[derive(Serialize)]
struct JsonReport<'a> {
    session_id: Option<&'a str>,
    transcript: &'a str,
    summary: JsonSummary,
    claims: Vec<JsonClaim<'a>>,
    test_integrity: Vec<JsonFinding<'a>>,
    checks_seen: Vec<JsonSeen<'a>>,
    unrecognised_commands: &'a [String],
}

#[derive(Serialize)]
struct JsonSeen<'a> {
    kind: &'a str,
    runner: &'a str,
    outcome: &'a str,
    evidence: Option<&'a str>,
}

#[derive(Serialize)]
struct JsonSummary {
    claims_checked: usize,
    problems: usize,
    warnings: usize,
    check_runs_seen: usize,
    headline: String,
}

#[derive(Serialize)]
struct JsonClaim<'a> {
    kind: &'a str,
    verdict: &'a str,
    reason: &'a str,
    quote: &'a str,
    in_final_message: bool,
    evidence: Option<&'a str>,
}

#[derive(Serialize)]
struct JsonFinding<'a> {
    severity: &'a str,
    kind: &'a str,
    path: &'a str,
    detail: &'a str,
    snippet: Option<&'a str>,
}

pub fn to_json(r: &Report) -> String {
    let doc = JsonReport {
        session_id: r.session_id.as_deref(),
        transcript: &r.transcript,
        summary: JsonSummary {
            claims_checked: r.checked.len(),
            problems: r.problems().len(),
            warnings: r.warnings().len(),
            check_runs_seen: r.runs_seen,
            headline: r.headline(),
        },
        claims: r
            .checked
            .iter()
            .map(|c| JsonClaim {
                kind: c.claim.kind.label(),
                verdict: c.verdict.label(),
                reason: &c.reason,
                quote: &c.claim.quote,
                in_final_message: c.claim.in_summary,
                evidence: c.evidence.as_deref(),
            })
            .collect(),
        test_integrity: r
            .findings
            .iter()
            .map(|f| JsonFinding {
                severity: match f.severity {
                    Severity::Warning => "warning",
                    Severity::Notice => "notice",
                },
                kind: f.kind,
                path: &f.path,
                detail: &f.detail,
                snippet: f.snippet.as_deref(),
            })
            .collect(),
        checks_seen: r
            .seen
            .iter()
            .map(|s| JsonSeen {
                kind: s.kind,
                runner: &s.runner,
                outcome: s.outcome,
                evidence: s.detail.as_deref(),
            })
            .collect(),
        unrecognised_commands: &r.unrecognised,
    };
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".into())
}

// ---------------------------------------------------------------- Terminal

struct Style {
    on: bool,
}

impl Style {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    fn red(&self, s: &str) -> String {
        self.paint("31;1", s)
    }
    fn yellow(&self, s: &str) -> String {
        self.paint("33;1", s)
    }
    fn green(&self, s: &str) -> String {
        self.paint("32;1", s)
    }
    fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }
}

fn verdict_marker(v: Verdict, st: &Style) -> String {
    match v {
        Verdict::Supported => st.green("✓ supported  "),
        Verdict::Qualified => st.yellow("~ qualified  "),
        Verdict::Unsupported => st.red("✗ unsupported"),
        Verdict::Contradicted => st.red("✗ contradicted"),
        Verdict::Inconclusive => st.dim("? inconclusive"),
    }
}

/// What backcheck saw, for when a verdict is surprising.
///
/// The usual reason a claim reads as unsupported is not that nothing ran, but that the tool
/// that ran is one backcheck does not know. Showing both lists makes that immediate.
pub fn to_explanation(r: &Report, color: bool) -> String {
    let st = Style { on: color };
    let mut out = String::new();

    out.push_str(&st.bold("  What backcheck saw\n"));
    if r.seen.is_empty() {
        out.push_str(&st.dim("    no checks recognised in this session\n"));
    }
    for s in &r.seen {
        let mark = match s.outcome {
            "passed" => st.green("passed          "),
            "failed" => st.red("failed          "),
            "did not complete" => st.yellow("did not complete"),
            _ => st.dim("unreadable      "),
        };
        out.push_str(&format!(
            "    {mark}  {} ({})\n",
            st.bold(&s.runner),
            s.kind
        ));
        if let Some(d) = &s.detail {
            out.push_str(&format!("        {}\n", st.dim(d)));
        }
    }

    if !r.unrecognised.is_empty() {
        out.push('\n');
        out.push_str(&st.bold("  Ran, but not recognised as a check\n"));
        out.push_str(
            &st.dim("    If one of these is a real check, backcheck is missing a runner for it.\n"),
        );
        for c in r.unrecognised.iter().take(15) {
            out.push_str(&format!("    {}\n", st.cyan(c)));
        }
        if r.unrecognised.len() > 15 {
            out.push_str(&st.dim(&format!("    (+{} more)\n", r.unrecognised.len() - 15)));
        }
    }
    out.push('\n');
    out
}

pub fn to_terminal(r: &Report, color: bool, verbose: bool) -> String {
    let st = Style { on: color };
    let mut out = String::new();

    out.push('\n');
    out.push_str(&st.bold("backcheck"));
    if let Some(id) = &r.session_id {
        out.push_str(&st.dim(&format!("  session {}", &id[..id.len().min(8)])));
    }
    out.push('\n');
    out.push_str(&st.dim(&format!("  {}\n\n", r.transcript)));

    if r.checked.is_empty() && r.findings.is_empty() {
        out.push_str(&st.dim("  No verifiable claims were made in this session.\n\n"));
        return out;
    }

    // Claims, problems first.
    let mut ordered: Vec<&Checked> = r.checked.iter().collect();
    ordered.sort_by_key(|c| std::cmp::Reverse(c.verdict));

    if !ordered.is_empty() {
        out.push_str(&st.bold("  Claims\n"));
        for c in ordered {
            if !verbose && c.verdict == Verdict::Supported {
                continue;
            }
            out.push_str(&format!(
                "  {}  {}\n",
                verdict_marker(c.verdict, &st),
                st.bold(c.claim.kind.label())
            ));
            out.push_str(&format!(
                "      {}\n",
                st.dim(&format!("“{}”", c.claim.quote))
            ));
            out.push_str(&format!("      {}\n", c.reason));
            if let Some(e) = &c.evidence {
                out.push_str(&format!("      {}\n", st.cyan(&format!("evidence: {e}"))));
            }
            out.push('\n');
        }
        let supported = r
            .checked
            .iter()
            .filter(|c| c.verdict == Verdict::Supported)
            .count();
        if !verbose && supported > 0 {
            out.push_str(&st.dim(&format!(
                "  ({supported} supported claim(s) hidden; use --verbose to show)\n\n"
            )));
        }
    }

    if !r.findings.is_empty() {
        out.push_str(&st.bold("  Test integrity\n"));
        for f in &r.findings {
            let marker = match f.severity {
                Severity::Warning => st.red("!"),
                Severity::Notice => st.yellow("·"),
            };
            out.push_str(&format!(
                "  {marker} {}  {}\n",
                st.bold(f.kind),
                st.dim(&f.path)
            ));
            out.push_str(&format!("      {}\n", f.detail));
            if let Some(s) = &f.snippet {
                out.push_str(&format!("      {}\n", st.cyan(s)));
            }
            out.push('\n');
        }
    }

    let head = r.headline();
    out.push_str(&if r.has_problems() {
        st.red(&format!("  {head}\n"))
    } else {
        st.green(&format!("  {head}\n"))
    });
    out.push('\n');
    out
}

/// The message handed back to Claude Code when a Stop hook blocks.
pub fn to_hook_reason(r: &Report) -> String {
    let mut lines = vec![
        "backcheck reviewed this session's transcript and could not verify some of what was reported:"
            .to_string(),
        String::new(),
    ];
    for c in r.problems() {
        lines.push(format!(
            "- {} ({}): {}",
            c.claim.kind.label(),
            c.verdict.label(),
            c.reason
        ));
        lines.push(format!("  claimed: “{}”", c.claim.quote));
    }
    for f in r.warnings() {
        lines.push(format!(
            "- test integrity: {} in {} — {}",
            f.kind, f.path, f.detail
        ));
    }
    lines.push(String::new());
    lines.push(
        "Please resolve each point: run the checks you referred to, or correct the summary so it \
         matches what actually happened."
            .to_string(),
    );
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::{Claim, ClaimKind};

    fn checked(kind: ClaimKind, verdict: Verdict) -> Checked {
        Checked {
            claim: Claim {
                kind,
                seq: 1,
                quote: "All tests pass.".into(),
                in_summary: true,
                subject: None,
            },
            verdict,
            reason: "no test run appears in this session".into(),
            evidence: None,
        }
    }

    fn report(checked: Vec<Checked>) -> Report {
        Report {
            session_id: Some("abcd1234efgh".into()),
            transcript: "/tmp/x.jsonl".into(),
            checked,
            findings: vec![],
            runs_seen: 0,
            seen: vec![],
            unrecognised: vec![],
        }
    }

    #[test]
    fn flags_problems() {
        let r = report(vec![checked(ClaimKind::TestsPass, Verdict::Unsupported)]);
        assert!(r.has_problems());
        assert_eq!(r.problems().len(), 1);
    }

    #[test]
    fn clean_report_has_no_problems() {
        let r = report(vec![checked(ClaimKind::TestsPass, Verdict::Supported)]);
        assert!(!r.has_problems());
        assert!(r.headline().contains("all supported"));
    }

    #[test]
    fn inconclusive_is_not_counted_as_supported() {
        // Regression: a report with three supported claims and one inconclusive one
        // summarised itself as "4 claims checked, all supported", overstating the check.
        let r = report(vec![
            checked(ClaimKind::TestsPass, Verdict::Supported),
            checked(ClaimKind::LintPasses, Verdict::Supported),
            checked(ClaimKind::Committed, Verdict::Supported),
            checked(ClaimKind::BuildPasses, Verdict::Inconclusive),
        ]);
        let head = r.headline();
        assert!(!r.has_problems(), "inconclusive is not a problem");
        assert!(
            !head.contains("all supported"),
            "must not claim everything was verified: {head}"
        );
        assert!(head.contains("3 claims supported"), "got: {head}");
        assert!(head.contains("1 could not be checked"), "got: {head}");
    }

    #[test]
    fn json_is_valid_and_complete() {
        let r = report(vec![checked(ClaimKind::TestsPass, Verdict::Unsupported)]);
        let v: serde_json::Value = serde_json::from_str(&to_json(&r)).unwrap();
        assert_eq!(v["summary"]["problems"], 1);
        assert_eq!(v["claims"][0]["verdict"], "unsupported");
    }

    #[test]
    fn terminal_output_is_plain_without_color() {
        let r = report(vec![checked(ClaimKind::TestsPass, Verdict::Unsupported)]);
        let s = to_terminal(&r, false, false);
        assert!(!s.contains('\x1b'));
        assert!(s.contains("unsupported"));
    }

    #[test]
    fn hook_reason_names_each_problem() {
        let r = report(vec![checked(ClaimKind::TestsPass, Verdict::Unsupported)]);
        let s = to_hook_reason(&r);
        assert!(s.contains("tests pass"));
        assert!(s.contains("no test run"));
    }
}
