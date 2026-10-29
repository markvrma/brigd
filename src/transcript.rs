//! claude's transcript files (~/.claude/projects/<slug>/<session>.jsonl and its subagents/):
//! where they are, the status, changed files and final text read from them, and mirroring
//! subagent transcripts into <agent>/<subagent>/output.md.

use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::{fs, io};

/// claude's ~/.claude/projects dir name for a cwd: every `/` and `.` becomes `-`.
pub(crate) fn slug(cwd: &Path) -> String {
    cwd.to_string_lossy().replace(['/', '.'], "-")
}

/// claude's ~/.claude/projects/<slug> dir for sessions run in `cwd`.
fn project_dir(home: &Path, cwd: &Path) -> PathBuf {
    home.join(".claude/projects").join(slug(cwd))
}

/// The transcript claude writes for `session` run in `cwd`.
pub(crate) fn transcript_path(home: &Path, cwd: &Path, session: &str) -> PathBuf {
    project_dir(home, cwd).join(format!("{session}.jsonl"))
}

/// Where claude keeps the subagent transcripts of a session.
pub(crate) fn subagents_dir(home: &Path, cwd: &Path, session: &str) -> PathBuf {
    project_dir(home, cwd).join(session).join("subagents")
}

/// Last user/assistant record: assistant end_turn => idle, anything else => working.
pub(crate) fn transcript_status(path: &Path) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(256 * 1024)))
        .ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v["type"].as_str() {
            Some("assistant") if v["message"]["stop_reason"] == "end_turn" => {
                return Some("idle".into())
            }
            Some("assistant" | "user") => return Some("working".into()),
            _ => {}
        }
    }
    None
}

/// The files a transcript changed (Edit/MultiEdit/Write results, and Bash edits'
/// bashEditDiff.files[]), in first-change order.
pub(crate) fn changed_files(jsonl: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for v in jsonl
        .lines()
        .filter(|l| l.contains("\"filePath\""))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    {
        let r = &v["toolUseResult"];
        let bash = r["bashEditDiff"]["files"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| &f["filePath"]);
        add(
            &mut out,
            bash.chain([&r["filePath"]])
                .filter_map(Value::as_str)
                .map(PathBuf::from)
                .collect(),
        );
    }
    out
}

/// The final text of a transcript: the last assistant end_turn record's text blocks.
fn final_text(jsonl: &str) -> Option<String> {
    let last = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .rfind(|v| v["type"] == "assistant" && v["message"]["stop_reason"] == "end_turn")?;
    let text: String = last["message"]["content"]
        .as_array()?
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    (!text.is_empty()).then_some(text)
}

/// Appends the files in `new` not yet in `out`.
pub(crate) fn add(out: &mut Vec<PathBuf>, new: Vec<PathBuf>) {
    for f in new {
        if !out.contains(&f) {
            out.push(f);
        }
    }
}

/// Dir name for a subagent: its description as a slug, plus a short id so it stays unique and stable.
fn sub_name(id: &str, desc: &str) -> String {
    let mut s: String = desc
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s: String = s.trim_matches('-').chars().take(32).collect();
    let s = s.trim_end_matches('-');
    format!(
        "{}-{}",
        if s.is_empty() { "agent" } else { s },
        &id[..id.len().min(7)]
    )
}

/// Mirrors `subagents` (claude's flat transcript dir) into nested dirs under
/// `agent_dir`, following meta.parentAgentId. Writes output.md once a subagent
/// has finished. Missing `subagents` dir = no subagents yet.
pub(crate) fn sync_subagents(subagents: &Path, agent_dir: &Path) -> io::Result<()> {
    // ponytail: only subagents/ itself; workflow agents under subagents/workflows/ are skipped.
    let mut metas: HashMap<String, (Option<String>, String)> = HashMap::new();
    for e in fs::read_dir(subagents).into_iter().flatten().flatten() {
        let f = e.file_name().to_string_lossy().into_owned();
        let Some(id) = f
            .strip_prefix("agent-")
            .and_then(|f| f.strip_suffix(".meta.json"))
        else {
            continue;
        };
        let Ok(m) = fs::read_to_string(e.path())
            .map(|s| serde_json::from_str::<Value>(&s).unwrap_or_default())
        else {
            continue;
        };
        let parent = m["parentAgentId"].as_str().map(String::from);
        metas.insert(
            id.into(),
            (parent, m["description"].as_str().unwrap_or("").into()),
        );
    }
    for id in metas.keys() {
        // Walk up to the top-level subagent; stop at unknown parents and at cycles.
        let mut chain = vec![id.as_str()];
        while let Some((Some(p), _)) = metas.get(*chain.last().unwrap()) {
            if !metas.contains_key(p) || chain.contains(&p.as_str()) || chain.len() > 16 {
                break;
            }
            chain.push(p);
        }
        let dir = chain.iter().rev().fold(agent_dir.to_path_buf(), |d, i| {
            d.join(sub_name(i, &metas[*i].1))
        });
        fs::create_dir_all(&dir)?;
        let jsonl =
            fs::read_to_string(subagents.join(format!("agent-{id}.jsonl"))).unwrap_or_default();
        if let Some(text) = final_text(&jsonl) {
            let out = dir.join("output.md");
            if fs::read_to_string(&out).ok().as_deref() != Some(text.as_str()) {
                fs::write(out, text)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::tests::tmp;

    fn end_turn(text: &str) -> String {
        serde_json::json!({"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":text}]}}).to_string()
    }

    #[test]
    fn changed_files_from_transcript() {
        let res = |r: Value| serde_json::json!({"type": "user", "toolUseResult": r}).to_string();
        let j = [
            res(serde_json::json!({"filePath": "/r/src/main.rs", "structuredPatch": []})),
            res(serde_json::json!({"type": "create", "filePath": "/r/new.md", "content": "x"})),
            r#"{"type":"user","toolUseResult":"Error: old_string not found"}"#.into(),
            res(serde_json::json!({"type": "text", "file": {"filePath": "/r/read.rs"}})), // a Read: not a change
            res(serde_json::json!({"filePath": "/r/src/main.rs"})),
            res(serde_json::json!({"stdout": "", "bashEditDiff": {"files": [{"filePath": "/r/src/main.rs"}, {"filePath": "/r/b.sh"}]}})),
        ]
        .join("\n");
        assert_eq!(
            changed_files(&j),
            ["/r/src/main.rs", "/r/new.md", "/r/b.sh"].map(PathBuf::from)
        );
    }

    #[test]
    fn slug_rule() {
        assert_eq!(
            slug(Path::new("/Users/alice/.brigd/x")),
            "-Users-alice--brigd-x"
        );
    }

    #[test]
    fn final_text_picks_last_end_turn() {
        let tool = r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"text","text":"no"}]}}"#;
        let j = [
            end_turn("first"),
            tool.into(),
            end_turn("last"),
            r#"{"type":"user"}"#.into(),
        ]
        .join("\n");
        assert_eq!(final_text(&j).as_deref(), Some("last"));
        assert_eq!(final_text(tool), None);
    }

    #[test]
    fn sync_nests_subagents() {
        let d = tmp("sync");
        let (subs, agent) = (d.join("subagents"), d.join("agent"));
        fs::create_dir_all(&subs).unwrap();
        let meta = |id: &str, m: Value| {
            fs::write(subs.join(format!("agent-{id}.meta.json")), m.to_string()).unwrap()
        };
        meta(
            "aaaaaaaa1",
            serde_json::json!({"description": "Find the bug!", "spawnDepth": 0}),
        );
        meta(
            "bbbbbbbb2",
            serde_json::json!({"description": "grep", "spawnDepth": 1, "parentAgentId": "aaaaaaaa1"}),
        );
        meta(
            "ccccccccc",
            serde_json::json!({"description": "", "spawnDepth": 0}),
        );
        fs::write(subs.join("agent-aaaaaaaa1.jsonl"), end_turn("top done")).unwrap();
        fs::write(subs.join("agent-bbbbbbbb2.jsonl"), end_turn("child done")).unwrap();
        fs::write(subs.join("agent-ccccccccc.jsonl"), r#"{"type":"user"}"#).unwrap();
        sync_subagents(&subs, &agent).unwrap();
        let top = agent.join("find-the-bug-aaaaaaa");
        assert_eq!(
            fs::read_to_string(top.join("output.md")).unwrap(),
            "top done"
        );
        assert_eq!(
            fs::read_to_string(top.join("grep-bbbbbbb/output.md")).unwrap(),
            "child done"
        );
        assert!(agent.join("agent-ccccccc").is_dir());
        assert!(!agent.join("agent-ccccccc/output.md").exists()); // still running
        sync_subagents(&d.join("missing"), &agent).unwrap();
        let _ = fs::remove_dir_all(&d);
    }
}
