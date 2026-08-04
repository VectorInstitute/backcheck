//! The evidence ledger: what the transcript proves actually happened.
//!
//! Built purely from tool calls and their recorded results. Nothing here is inferred from what
//! the assistant *said* -- that is the whole point of the separation.

use crate::runners::{self, CheckKind, CheckRun, Outcome};
use crate::transcript::{ToolCall, Transcript};

/// A file the agent wrote to, and how.
#[derive(Debug, Clone)]
pub struct FileWrite {
    pub seq: usize,
    pub path: String,
    /// `Write` creates or replaces; `Edit` patches.
    pub tool: String,
    /// Text removed by an Edit, when recorded.
    pub old_string: Option<String>,
    /// Text inserted by an Edit, or full content for a Write.
    pub new_string: Option<String>,
    pub succeeded: bool,
}

impl FileWrite {
    pub fn is_test_file(&self) -> bool {
        is_test_path(&self.path)
    }
}

/// Files whose contents cannot change what a test run would do.
///
/// Editing a README after a green suite does not make the suite stale, and saying it does is
/// exactly the false alarm that gets a hook uninstalled. Configuration and lockfiles are
/// deliberately absent from this list: those can and do change behaviour.
pub fn is_documentation(path: &str) -> bool {
    let p = path.to_lowercase();
    let name = p.rsplit(['/', '\\']).next().unwrap_or(&p);
    const DOC_EXT: &[&str] = &[
        ".md",
        ".markdown",
        ".rst",
        ".txt",
        ".adoc",
        ".png",
        ".jpg",
        ".jpeg",
        ".gif",
        ".svg",
        ".pdf",
        ".ico",
        ".webp",
        ".csv",
    ];
    DOC_EXT.iter().any(|e| name.ends_with(e))
        || matches!(
            name,
            "license" | "licence" | "notice" | "authors" | "codeowners" | ".gitignore"
        )
}

/// Heuristic: does this path look like a test file?
pub fn is_test_path(path: &str) -> bool {
    let p = path.to_lowercase();
    let name = p.rsplit(['/', '\\']).next().unwrap_or(&p);
    name.starts_with("test_")
        || name.ends_with("_test.py")
        || name.ends_with("_test.go")
        || name.ends_with("_test.rs")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.ends_with("_spec.rb")
        || p.contains("/tests/")
        || p.contains("/test/")
        || p.contains("/spec/")
        || p.starts_with("tests/")
        || p.starts_with("test/")
}

/// A git operation the agent performed.
#[derive(Debug, Clone)]
pub struct GitOp {
    pub seq: usize,
    pub kind: GitOpKind,
    pub command: String,
    pub succeeded: bool,
    pub output_line: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitOpKind {
    Commit,
    Push,
}

#[derive(Debug, Default)]
pub struct Ledger {
    pub checks: Vec<CheckRun>,
    pub writes: Vec<FileWrite>,
    pub git_ops: Vec<GitOp>,
}

impl Ledger {
    pub fn build(transcript: &Transcript) -> Self {
        let mut ledger = Ledger::default();

        for call in &transcript.tool_calls {
            match call.name.as_str() {
                "Bash" => ledger.ingest_bash(call),
                "Edit" | "Write" | "NotebookEdit" => ledger.ingest_write(call),
                _ => {}
            }
        }
        ledger
    }

    fn ingest_bash(&mut self, call: &ToolCall) {
        let Some(command) = call.input_str("command") else {
            return;
        };
        let (interrupted, is_error) = call
            .result
            .as_ref()
            .map(|r| (r.interrupted, r.is_error))
            .unwrap_or((false, false));
        let output = call.output();

        self.checks.extend(runners::analyse(
            call.seq,
            command,
            &output,
            interrupted,
            is_error,
        ));

        for segment in runners::split_segments(command) {
            let lower = segment.to_lowercase();
            let kind = if lower.starts_with("git commit") || lower.contains(" git commit") {
                GitOpKind::Commit
            } else if lower.starts_with("git push") || lower.contains(" git push") {
                GitOpKind::Push
            } else {
                continue;
            };
            // Git reports failures in its output; a blocked call never ran at all.
            let failed_markers = [
                "nothing to commit",
                "no changes added to commit",
                "rejected",
                "failed to push",
                "error:",
                "fatal:",
            ];
            let lower_out = output.to_lowercase();
            let succeeded =
                !is_error && !interrupted && !failed_markers.iter().any(|m| lower_out.contains(m));
            self.git_ops.push(GitOp {
                seq: call.seq,
                kind,
                command: segment,
                succeeded,
                output_line: output
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .map(|l| l.trim().to_string()),
            });
        }
    }

    fn ingest_write(&mut self, call: &ToolCall) {
        let structured = call.result.as_ref().and_then(|r| r.structured.as_ref());
        let path = call
            .input_str("file_path")
            .map(String::from)
            .or_else(|| {
                structured
                    .and_then(|s| s.get("filePath"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .unwrap_or_default();
        if path.is_empty() {
            return;
        }
        let succeeded = call.result.as_ref().map(|r| !r.is_error).unwrap_or(false);

        self.writes.push(FileWrite {
            seq: call.seq,
            path,
            tool: call.name.clone(),
            old_string: call.input_str("old_string").map(String::from),
            new_string: call
                .input_str("new_string")
                .or_else(|| call.input_str("content"))
                .map(String::from),
            succeeded,
        });
    }

    /// The last run of a given kind, whatever its outcome.
    pub fn last_check(&self, kind: CheckKind) -> Option<&CheckRun> {
        self.checks.iter().rfind(|c| c.kind == kind)
    }

    /// All runs of a kind.
    pub fn checks_of(&self, kind: CheckKind) -> Vec<&CheckRun> {
        self.checks.iter().filter(|c| c.kind == kind).collect()
    }

    /// Sequence number of the last successful write to a non-test source file.
    ///
    /// Used for staleness: a passing run that predates the newest edit proves nothing about the
    /// code as it now stands.
    pub fn last_source_write(&self) -> Option<&FileWrite> {
        self.writes
            .iter()
            .rfind(|w| w.succeeded && !w.is_test_file())
    }

    /// Writes that landed after the given point in the transcript.
    pub fn writes_after(&self, seq: usize) -> Vec<&FileWrite> {
        self.writes
            .iter()
            .filter(|w| w.succeeded && w.seq > seq)
            .collect()
    }

    pub fn last_git_op(&self, kind: GitOpKind) -> Option<&GitOp> {
        self.git_ops.iter().rfind(|g| g.kind == kind)
    }

    /// Did any check of this kind actually pass cleanly?
    pub fn has_clean_pass(&self, kind: CheckKind) -> bool {
        self.checks
            .iter()
            .any(|c| c.kind == kind && c.is_clean_pass())
    }

    /// Did any check of this kind end in failure?
    pub fn has_failure(&self, kind: CheckKind) -> bool {
        self.checks
            .iter()
            .any(|c| c.kind == kind && c.outcome == Outcome::Failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Transcript;

    fn ledger_from(lines: &[serde_json::Value]) -> Ledger {
        let raw = lines
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        Ledger::build(&Transcript::parse_str(&raw))
    }

    fn bash(id: &str, cmd: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "assistant",
            "message": {"content": [{"type": "tool_use", "id": id, "name": "Bash",
                                     "input": {"command": cmd}}]}
        })
    }

    fn result(id: &str, stdout: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "user",
            "toolUseResult": {"stdout": stdout, "stderr": "", "interrupted": false},
            "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": stdout}]}
        })
    }

    fn edit(id: &str, path: &str, old: &str, new: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "assistant",
            "message": {"content": [{"type": "tool_use", "id": id, "name": "Edit",
                                     "input": {"file_path": path, "old_string": old, "new_string": new}}]}
        })
    }

    #[test]
    fn records_test_runs() {
        let l = ledger_from(&[bash("1", "pytest -q"), result("1", "5 passed")]);
        assert_eq!(l.checks.len(), 1);
        assert!(l.has_clean_pass(CheckKind::Test));
    }

    #[test]
    fn records_git_commit_success_and_failure() {
        let ok = ledger_from(&[
            bash("1", "git commit -m x"),
            result("1", "[main abc123] x\n 1 file changed"),
        ]);
        assert!(ok.last_git_op(GitOpKind::Commit).unwrap().succeeded);

        let bad = ledger_from(&[
            bash("1", "git commit -m x"),
            result("1", "nothing to commit"),
        ]);
        assert!(!bad.last_git_op(GitOpKind::Commit).unwrap().succeeded);
    }

    #[test]
    fn tracks_writes_and_distinguishes_test_files() {
        let l = ledger_from(&[
            edit("1", "/repo/src/main.rs", "a", "b"),
            result("1", "ok"),
            edit("2", "/repo/tests/test_main.py", "c", "d"),
            result("2", "ok"),
        ]);
        assert_eq!(l.writes.len(), 2);
        assert!(!l.writes[0].is_test_file());
        assert!(l.writes[1].is_test_file());
        assert_eq!(l.last_source_write().unwrap().path, "/repo/src/main.rs");
    }

    #[test]
    fn detects_edits_after_a_test_run() {
        let l = ledger_from(&[
            bash("1", "pytest"),
            result("1", "5 passed"),
            edit("2", "/repo/src/main.rs", "a", "b"),
            result("2", "ok"),
        ]);
        let run = l.last_check(CheckKind::Test).unwrap();
        assert_eq!(l.writes_after(run.seq).len(), 1);
    }

    #[test]
    fn test_path_heuristics() {
        assert!(is_test_path("tests/test_foo.py"));
        assert!(is_test_path("src/foo.test.ts"));
        assert!(is_test_path("pkg/foo_test.go"));
        assert!(is_test_path("spec/models/user_spec.rb"));
        assert!(!is_test_path("src/latest.rs"));
        assert!(!is_test_path("src/contest.py"));
    }
}
