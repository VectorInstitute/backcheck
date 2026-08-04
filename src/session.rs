//! Locating session transcripts on disk.
//!
//! Claude Code stores transcripts under `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`,
//! where the directory name is the working directory with path separators replaced by dashes.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

pub fn projects_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)?;
    Some(home.join(".claude").join("projects"))
}

/// Encode a working directory the way Claude Code names its project folders.
pub fn encode_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy().replace('\\', "/");
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        out.push(if ch == '/' || ch == '.' || ch == '_' {
            '-'
        } else {
            ch
        });
    }
    out
}

/// The most recently modified transcript for a directory, or across all projects.
pub fn latest_transcript(cwd: Option<&Path>) -> Result<PathBuf> {
    let root = projects_dir().ok_or_else(|| anyhow!("could not locate ~/.claude/projects"))?;
    if !root.exists() {
        return Err(anyhow!(
            "no Claude Code sessions found at {} — has Claude Code run on this machine?",
            root.display()
        ));
    }

    let dirs: Vec<PathBuf> = match cwd {
        Some(dir) => {
            let encoded = root.join(encode_cwd(dir));
            if !encoded.exists() {
                return Err(anyhow!(
                    "no sessions recorded for {} (looked in {})",
                    dir.display(),
                    encoded.display()
                ));
            }
            vec![encoded]
        }
        None => std::fs::read_dir(&root)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
    };

    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                best = Some((modified, path));
            }
        }
    }

    best.map(|(_, p)| p)
        .ok_or_else(|| anyhow!("no .jsonl transcripts found under {}", root.display()))
}

/// All transcripts for a directory, newest first.
pub fn transcripts_for(cwd: &Path) -> Result<Vec<PathBuf>> {
    let root = projects_dir().ok_or_else(|| anyhow!("could not locate ~/.claude/projects"))?;
    let dir = root.join(encode_cwd(cwd));
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut items: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .filter_map(|e| {
            let m = e.metadata().ok()?.modified().ok()?;
            Some((m, e.path()))
        })
        .collect();
    items.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    Ok(items.into_iter().map(|(_, p)| p).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_paths_like_claude_code() {
        assert_eq!(
            encode_cwd(Path::new("/Users/ann/src/my-app")),
            "-Users-ann-src-my-app"
        );
        assert_eq!(
            encode_cwd(Path::new("/home/bo/proj.v2")),
            "-home-bo-proj-v2"
        );
    }
}
