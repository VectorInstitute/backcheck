//! Turning a raw shell command into the pieces backcheck can reason about.
//!
//! Three jobs live here: splitting a chained command into its steps, stripping the wrappers a
//! runner arrives behind, and working out which part of the combined output each step produced.

use regex::Regex;
use std::sync::OnceLock;

use super::re;

/// How one command in a chain is joined to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Join {
    /// `&&`: the next command runs only if this one exited zero.
    AndThen,
    /// `||`: the next command runs only if this one failed.
    OrElse,
    /// `;` or a newline: the next command runs regardless.
    Then,
}

/// Split a shell line into segments, keeping the operator that follows each one.
///
/// The operator is worth preserving because `&&` carries a proof: if the command after it
/// produced output, the one before it must have exited zero.
///
/// Pipelines are kept intact: `pytest | tail` is one command whose output happens to be filtered.
pub(crate) fn split_with_joins(command: &str) -> Vec<(String, Join)> {
    let mut out: Vec<(String, Join)> = Vec::new();
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
                let join = if c == '&' {
                    Join::AndThen
                } else {
                    Join::OrElse
                };
                out.push((std::mem::take(&mut cur), join));
                continue;
            }
            if c == ';' || c == '\n' {
                out.push((std::mem::take(&mut cur), Join::Then));
                continue;
            }
        }
        cur.push(c);
    }

    out.push((cur, Join::Then));
    out.into_iter()
        .map(|(s, j)| (s.trim().to_string(), j))
        .filter(|(s, _)| !s.is_empty())
        .collect()
}

/// Split a shell line into segments that each start a fresh command.
pub fn split_segments(command: &str) -> Vec<String> {
    split_with_joins(command)
        .into_iter()
        .map(|(s, _)| s)
        .collect()
}

/// Reduce a command to the tool that actually runs.
///
/// Real sessions rarely invoke a runner by its bare name. It arrives behind an environment
/// (`uv run pytest`), an interpreter (`python -m pytest`), or -- most commonly of all -- an
/// absolute or virtualenv-relative path (`.venv/bin/python -m pytest`, `./node_modules/.bin/jest`).
/// Each of those has to collapse to `pytest` or `jest`, or the run is invisible and an honest
/// claim gets reported as unsupported.
pub(crate) fn strip_invocation(segment: &str) -> String {
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
            "do ",
        ] {
            if let Some(rest) = s.strip_prefix(prefix) {
                s = rest.trim_start().to_string();
            }
        }

        // `timeout 240 uv run pytest`, `timeout --signal=KILL 30 cargo test`. Long suites are
        // routinely wrapped this way, and missing it hides the run entirely.
        static TIMEOUT: OnceLock<Regex> = OnceLock::new();
        let timeout = re(&TIMEOUT, r"^timeout\s+(?:-[-\w=]+\s+)*[\d.]+[smhd]?\s+");
        if let Some(m) = timeout.find(&s) {
            s = s[m.end()..].trim_start().to_string();
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

/// A slice of a command's output, and whether it is known to belong to that command alone.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Region<'a> {
    pub(crate) text: &'a str,
    /// True when a landmark bounded this text, or the command had only one step.
    pub(crate) exclusive: bool,
}

/// Split a chained command's output into the region produced by each segment.
///
/// Agents habitually separate the parts of a chained command with `echo "=== lint ==="`, which
/// leaves a findable landmark in the combined output. Where those landmarks can be located,
/// each check is parsed against only the text it actually produced. Without them every parser
/// sees the whole stream, and a linter's "All checks passed!" can end up cited as the evidence
/// for a claim about tests.
pub(crate) fn attribute_output<'a>(segments: &[String], output: &'a str) -> Vec<Region<'a>> {
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

    // A command with a single step owns all of its output; otherwise a landmark on either
    // side is what makes the slice trustworthy.
    let single_step = segments.len() == 1;

    segments
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let before = marks[..i].iter().rev().flatten().next();
            let after = marks[i + 1..].iter().flatten().next();
            let start = before.map(|(_, end)| *end).unwrap_or(0);
            let end = after.map(|(begin, _)| *begin).unwrap_or(output.len());
            let bounded = before.is_some() || after.is_some();
            let text = if start >= end {
                ""
            } else {
                output.get(start..end).unwrap_or(output)
            };
            Region {
                text,
                exclusive: single_step || bounded,
            }
        })
        .collect()
}
