//! Streaming parser for Claude Code session transcripts (JSONL).
//!
//! A transcript is a newline-delimited sequence of records. The ones that matter here:
//!
//! - `{"type":"assistant","message":{"content":[{"type":"text",...},{"type":"tool_use",...}]}}`
//! - `{"type":"user","message":{"content":[{"type":"tool_result",...}]},"toolUseResult":{...}}`
//!
//! Tool results arrive on `user` records and carry two payloads: the `tool_result` content
//! block (what the model saw) and a top-level `toolUseResult` (structured metadata Claude Code
//! records alongside it). The structured payload is richer -- for `Bash` it separates stdout
//! from stderr and flags interruption -- so it is preferred when present.
//!
//! Note that exit codes are **not** recorded in the transcript. Whether a command succeeded has
//! to be recovered from its output, which is what [`crate::runners`] does.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

/// One tool invocation paired with its result, in transcript order.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Position in the event stream. Used for ordering comparisons (staleness).
    pub seq: usize,
    pub id: String,
    pub name: String,
    pub input: Value,
    pub result: Option<ToolResult>,
    pub timestamp: Option<String>,
}

impl ToolCall {
    /// A string input field, e.g. `command` for Bash or `file_path` for Edit.
    pub fn input_str(&self, key: &str) -> Option<&str> {
        self.input.get(key).and_then(Value::as_str)
    }

    /// Combined stdout/stderr of the call, however the transcript happened to record it.
    pub fn output(&self) -> String {
        match &self.result {
            Some(r) => r.text(),
            None => String::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolResult {
    /// The `tool_result` content block as plain text.
    pub content: String,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    /// True when the user or the harness cut the command short.
    pub interrupted: bool,
    /// True when the tool call itself failed (blocked, denied, invalid args).
    pub is_error: bool,
    /// Structured `toolUseResult` payload, when present.
    pub structured: Option<Value>,
}

impl ToolResult {
    pub fn text(&self) -> String {
        match (&self.stdout, &self.stderr) {
            (Some(o), Some(e)) if !e.trim().is_empty() => format!("{o}\n{e}"),
            (Some(o), _) => o.clone(),
            (None, Some(e)) => e.clone(),
            (None, None) => self.content.clone(),
        }
    }
}

/// A block of assistant prose. Claims are extracted from these.
#[derive(Debug, Clone)]
pub struct AssistantText {
    pub seq: usize,
    pub text: String,
    pub timestamp: Option<String>,
    /// True if this is the final assistant message of the session -- the summary the user reads,
    /// where unsupported claims do the most damage.
    pub is_last: bool,
}

#[derive(Debug, Default)]
pub struct Transcript {
    pub tool_calls: Vec<ToolCall>,
    pub assistant_texts: Vec<AssistantText>,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
}

impl Transcript {
    pub fn parse_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading transcript {}", path.display()))?;
        Ok(Self::parse_str(&raw))
    }

    pub fn parse_str(raw: &str) -> Self {
        let mut t = Transcript::default();
        let mut seq = 0usize;
        // tool_use id -> index into tool_calls, so results can be attached when they arrive later.
        let mut by_id: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(rec) = serde_json::from_str::<Value>(line) else {
                continue; // A partially written trailing line is normal for a live session.
            };

            if t.session_id.is_none() {
                t.session_id = rec
                    .get("sessionId")
                    .or_else(|| rec.get("session_id"))
                    .and_then(Value::as_str)
                    .map(String::from);
            }
            if t.cwd.is_none() {
                t.cwd = rec.get("cwd").and_then(Value::as_str).map(String::from);
            }

            let kind = rec.get("type").and_then(Value::as_str).unwrap_or("");
            let timestamp = rec
                .get("timestamp")
                .and_then(Value::as_str)
                .map(String::from);
            let Some(content) = rec
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            else {
                continue;
            };

            for block in content {
                let btype = block.get("type").and_then(Value::as_str).unwrap_or("");
                match (kind, btype) {
                    ("assistant", "text") => {
                        let text = block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        if !text.trim().is_empty() {
                            seq += 1;
                            t.assistant_texts.push(AssistantText {
                                seq,
                                text,
                                timestamp: timestamp.clone(),
                                is_last: false,
                            });
                        }
                    }
                    ("assistant", "tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        seq += 1;
                        by_id.insert(id.clone(), t.tool_calls.len());
                        t.tool_calls.push(ToolCall {
                            seq,
                            id,
                            name: block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            input: block.get("input").cloned().unwrap_or(Value::Null),
                            result: None,
                            timestamp: timestamp.clone(),
                        });
                    }
                    (_, "tool_result") => {
                        let id = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let Some(&idx) = by_id.get(id) else { continue };
                        let structured = rec.get("toolUseResult").cloned();
                        let mut res = ToolResult {
                            content: stringify_content(block.get("content")),
                            is_error: block
                                .get("is_error")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            ..Default::default()
                        };
                        if let Some(s) = &structured {
                            res.stdout = s.get("stdout").and_then(Value::as_str).map(String::from);
                            res.stderr = s.get("stderr").and_then(Value::as_str).map(String::from);
                            res.interrupted = s
                                .get("interrupted")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                        }
                        // Blocked or denied calls come back as a bare string, not an object.
                        if res.content.contains("<tool_use_error>") {
                            res.is_error = true;
                        }
                        res.structured = structured;
                        t.tool_calls[idx].result = Some(res);
                    }
                    _ => {}
                }
            }
        }

        if let Some(last) = t.assistant_texts.last_mut() {
            last.is_last = true;
        }
        t
    }

    /// Tool calls of a given name, in order.
    pub fn calls_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a ToolCall> + 'a {
        self.tool_calls.iter().filter(move |c| c.name == name)
    }

    /// Highest sequence number in the transcript, for "did anything happen after X" checks.
    pub fn max_seq(&self) -> usize {
        self.tool_calls
            .last()
            .map(|c| c.seq)
            .max(self.assistant_texts.last().map(|a| a.seq))
            .unwrap_or(0)
    }
}

/// A `tool_result` content field is either a string or a list of typed blocks.
fn stringify_content(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
{"type":"assistant","timestamp":"2026-01-01T00:00:00Z","cwd":"/repo","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"pytest -q"}}]}}
{"type":"user","toolUseResult":{"stdout":"3 passed in 0.1s","stderr":"","interrupted":false},"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"3 passed in 0.1s"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"All tests pass."}]}}
"#;

    #[test]
    fn pairs_tool_calls_with_results() {
        let t = Transcript::parse_str(SAMPLE);
        assert_eq!(t.tool_calls.len(), 1);
        let call = &t.tool_calls[0];
        assert_eq!(call.name, "Bash");
        assert_eq!(call.input_str("command"), Some("pytest -q"));
        assert!(call.output().contains("3 passed"));
    }

    #[test]
    fn marks_final_assistant_message() {
        let t = Transcript::parse_str(SAMPLE);
        assert_eq!(t.assistant_texts.len(), 1);
        assert!(t.assistant_texts[0].is_last);
    }

    #[test]
    fn tolerates_truncated_trailing_line() {
        let t = Transcript::parse_str(&format!("{SAMPLE}\n{{\"type\":\"assis"));
        assert_eq!(t.tool_calls.len(), 1);
    }

    #[test]
    fn detects_blocked_tool_call() {
        let raw = r#"
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"rm -rf /"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"<tool_use_error>Blocked</tool_use_error>"}]}}
"#;
        let t = Transcript::parse_str(raw);
        assert!(t.tool_calls[0].result.as_ref().unwrap().is_error);
    }
}
