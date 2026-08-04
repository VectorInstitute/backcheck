//! Stop-hook mode and hook installation.
//!
//! As a `Stop` hook, backcheck runs the moment Claude Code finishes and, when it finds claims the
//! transcript does not support, returns `decision: "block"` with a reason. Claude Code then hands
//! that reason back to the model, which has to resolve it before finishing.
//!
//! `stop_hook_active` guards the obvious hazard: if the model is already responding to a previous
//! block, blocking again could loop. In that case the findings are reported without blocking.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

/// The payload Claude Code writes to a Stop hook's stdin.
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct HookInput {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// True when this Stop hook is firing because a previous one blocked.
    #[serde(default)]
    pub stop_hook_active: bool,
    #[serde(default)]
    pub hook_event_name: Option<String>,
}

impl HookInput {
    pub fn from_stdin() -> Result<Self> {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading hook payload from stdin")?;
        if buf.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&buf).context("parsing hook payload as JSON")
    }

    pub fn transcript(&self) -> Option<PathBuf> {
        self.transcript_path.as_ref().map(PathBuf::from)
    }
}

/// The JSON a Stop hook writes to stdout to block completion.
pub fn block_output(reason: &str) -> String {
    json!({
        "decision": "block",
        "reason": reason,
    })
    .to_string()
}

/// Non-blocking hook output: let the turn finish, but surface the summary.
pub fn allow_output() -> String {
    json!({ "continue": true }).to_string()
}

// ---------------------------------------------------------------- install

fn settings_path(global: bool) -> Result<PathBuf> {
    if global {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .context("could not locate your home directory")?;
        Ok(PathBuf::from(home).join(".claude").join("settings.json"))
    } else {
        // settings.local.json, not settings.json: the latter is the file teams commit, and a
        // hook pointing at a binary a colleague has not installed fails for them on every turn.
        Ok(PathBuf::from(".claude").join("settings.local.json"))
    }
}

/// Add backcheck as a Stop hook in Claude Code's settings, preserving anything already there.
pub fn install(global: bool, blocking: bool) -> Result<PathBuf> {
    let path = settings_path(global)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let mut settings: serde_json::Value = if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        if raw.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&raw).with_context(|| {
                format!(
                    "{} is not valid JSON; fix it before installing",
                    path.display()
                )
            })?
        }
    } else {
        json!({})
    };

    // Advisory is the default. A hook that interrupts roughly one turn in four gets
    // uninstalled, and the people who have shipped these gates report that repeated
    // interruptions teach the model to route around them rather than to verify.
    let command = if blocking {
        "backcheck hook".to_string()
    } else {
        "backcheck hook --no-block".to_string()
    };

    let hooks = settings
        .as_object_mut()
        .context("settings.json must contain a JSON object")?
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let stop = hooks
        .as_object_mut()
        .context("`hooks` must be a JSON object")?
        .entry("Stop")
        .or_insert_with(|| json!([]));
    let matchers = stop
        .as_array_mut()
        .context("`hooks.Stop` must be an array")?;

    // Replace any existing backcheck entry rather than stacking duplicates.
    matchers.retain(|m| {
        !m.get("hooks").and_then(|h| h.as_array()).is_some_and(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.contains("backcheck"))
            })
        })
    });
    matchers.push(json!({
        "hooks": [{ "type": "command", "command": command }]
    }));

    let rendered = serde_json::to_string_pretty(&settings)? + "\n";
    std::fs::write(&path, rendered).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Remove backcheck from Claude Code's settings.
pub fn uninstall(global: bool) -> Result<Option<PathBuf>> {
    let path = settings_path(global)?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)?;
    let mut settings: serde_json::Value = serde_json::from_str(&raw)?;

    let removed = settings
        .get_mut("hooks")
        .and_then(|h| h.get_mut("Stop"))
        .and_then(|s| s.as_array_mut())
        .map(|matchers| {
            let before = matchers.len();
            matchers.retain(|m| {
                !m.get("hooks").and_then(|h| h.as_array()).is_some_and(|hs| {
                    hs.iter().any(|h| {
                        h.get("command")
                            .and_then(|c| c.as_str())
                            .is_some_and(|c| c.contains("backcheck"))
                    })
                })
            });
            before != matchers.len()
        })
        .unwrap_or(false);

    if !removed {
        return Ok(None);
    }
    std::fs::write(&path, serde_json::to_string_pretty(&settings)? + "\n")?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stop_hook_payload() {
        let raw = r#"{"session_id":"abc","transcript_path":"/t/x.jsonl","cwd":"/repo","stop_hook_active":false,"hook_event_name":"Stop"}"#;
        let h: HookInput = serde_json::from_str(raw).unwrap();
        assert_eq!(h.session_id.as_deref(), Some("abc"));
        assert!(!h.stop_hook_active);
        assert_eq!(h.transcript().unwrap().to_str().unwrap(), "/t/x.jsonl");
    }

    #[test]
    fn tolerates_unknown_fields() {
        let h: HookInput = serde_json::from_str(r#"{"session_id":"a","future_field":1}"#).unwrap();
        assert_eq!(h.session_id.as_deref(), Some("a"));
    }

    #[test]
    fn block_output_is_valid_hook_json() {
        let v: serde_json::Value = serde_json::from_str(&block_output("because")).unwrap();
        assert_eq!(v["decision"], "block");
        assert_eq!(v["reason"], "because");
    }

    #[test]
    fn install_preserves_existing_settings() {
        let dir = std::env::temp_dir().join(format!("backcheck-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        let settings = dir.join(".claude").join("settings.local.json");
        std::fs::write(
            &settings,
            r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"other"}]}]}}"#,
        )
        .unwrap();

        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        install(false, true).unwrap();
        install(false, true).unwrap(); // twice: must not duplicate
        std::env::set_current_dir(prev).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(v["model"], "opus", "unrelated settings must survive");
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "existing hook kept, backcheck added once");
        std::fs::remove_dir_all(&dir).ok();
    }
}
