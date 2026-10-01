//! The sidebar tree, read from disk: space (dir basename) → thread → agent →
//! [output.md, subagent dirs → output.md …, changed files]. Also mirrors claude's
//! subagent transcripts into <agent>/<subagent>/output.md.

use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Space,
    Thread,
    Agent,
    Subagent,
    Output,
    /// A file the agent changed; opens as a diff (see `diff_view`).
    Diff,
    /// The thread's "stray agents" dir: claude sessions started from a brigd terminal.
    Folder,
}

/// <thread>/stray agents/<name>/{cwd,session,base,pid,status}: claude sessions typed in a brigd terminal.
pub const STRAY: &str = "stray agents";

/// An agent's registry/state name: its label, or "stray agents/<label>" for a stray.
pub fn agent_id(path: &Path, label: &str) -> String {
    if path.parent().is_some_and(|p| p.ends_with(STRAY)) { format!("{STRAY}/{label}") } else { label.into() }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: Kind,
    pub label: String,
    /// Space: its cwd. Thread/Agent/Subagent: its dir. Output: the output.md file.
    /// Diff: <agent dir>/.diff/<changed file's absolute path>, a key for `diff_view`.
    pub path: PathBuf,
    pub children: Vec<Node>,
    /// Agent: its 0-based stage index in the flow.
    pub stage: Option<usize>,
}

/// claude's ~/.claude/projects dir name for a cwd: every `/` and `.` becomes `-`.
pub fn slug(cwd: &Path) -> String {
    cwd.to_string_lossy().replace(['/', '.'], "-")
}

/// The directory a space's agents run in: the repo for key "", else its worktree.
pub fn space_cwd(root: &Path, repo: &Path, thread: &str, key: &str) -> PathBuf {
    if key.is_empty() { repo.into() } else { root.join("worktrees").join(thread).join(key) }
}

/// Where claude keeps the subagent transcripts of a session.
pub fn subagents_dir(home: &Path, cwd: &Path, session: &str) -> PathBuf {
    home.join(".claude/projects").join(slug(cwd)).join(session).join("subagents")
}

/// Builds the sidebar from `root` (~/.brigd). A thread whose agents use several
/// spaces shows under each of them, holding only that space's agents.
pub fn build(root: &Path) -> Vec<Node> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let mut spaces: BTreeMap<PathBuf, Vec<Node>> = BTreeMap::new();
    let mut threads: Vec<PathBuf> = fs::read_dir(root.join("threads")).into_iter().flatten().flatten().map(|e| e.path()).collect();
    threads.sort();
    for tdir in threads {
        let read = |f: &str| fs::read_to_string(tdir.join(f)).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok());
        let (Some(flow), Some(state)) = (read("flowmap.json").or_else(|| read("flow.json")), read("state.json")) else { continue };
        let thread = tdir.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let repo = PathBuf::from(state["repo"].as_str().unwrap_or_default());
        let mut per_space: BTreeMap<PathBuf, Vec<Node>> = BTreeMap::new();
        for (si, a) in flow["stages"].as_array().into_iter().flatten().enumerate().flat_map(|(i, s)| s.as_array().into_iter().flatten().map(move |a| (i, a))) {
            // Only agents that have been launched: set_agent puts them in state.json's "agents".
            let Some(name) = a["name"].as_str().filter(|n| !state["agents"][*n].is_null()) else { continue };
            let cwd = space_cwd(root, &repo, &thread, a["worktree"].as_str().unwrap_or(""));
            let mut agent = Node { stage: Some(si), ..dir_node(Kind::Agent, name, &tdir.join(name)) };
            agent.children.extend(diff_nodes(&home, &cwd, &repo, &agent.path));
            per_space.entry(cwd).or_default().push(agent);
        }
        // Strays go in the deepest of the thread's spaces holding their cwd, else a space of their own.
        let mut strays: BTreeMap<PathBuf, Vec<Node>> = BTreeMap::new();
        let mut sdirs: Vec<PathBuf> = fs::read_dir(tdir.join(STRAY)).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        sdirs.sort();
        for d in sdirs {
            let Ok(cwd) = fs::read_to_string(d.join("cwd")).map(|c| PathBuf::from(c.trim())) else { continue };
            let mut agent = dir_node(Kind::Agent, &d.file_name().unwrap_or_default().to_string_lossy(), &d);
            agent.children.extend(diff_nodes(&home, &cwd, &cwd, &d));
            let space = per_space.keys().chain([&repo]).filter(|s| cwd.starts_with(s)).max_by_key(|s| s.components().count()).cloned();
            strays.entry(space.unwrap_or(cwd)).or_default().push(agent);
        }
        if per_space.is_empty() {
            per_space.insert(repo.clone(), vec![]); // nothing started yet: the thread (and its FLOWPLAN) under the repo
        }
        for s in strays.keys() {
            per_space.entry(s.clone()).or_default();
        }
        for (cwd, mut agents) in per_space {
            if let Some(s) = strays.remove(&cwd) {
                agents.insert(0, Node { kind: Kind::Folder, label: STRAY.into(), path: tdir.join(STRAY), children: s, stage: None });
            }
            // FLOWPLAN (the plan + live run log) is the thread's first child, its strays next.
            let plan = tdir.join("FLOWPLAN");
            if plan.exists() {
                agents.insert(0, Node { kind: Kind::Output, label: "FLOWPLAN".into(), path: plan, children: vec![], stage: None });
            }
            spaces.entry(cwd).or_default().push(Node { kind: Kind::Thread, label: thread.clone(), path: tdir.clone(), children: agents, stage: None });
        }
    }
    spaces
        .into_iter()
        .map(|(cwd, children)| {
            let label = cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| cwd.to_string_lossy().into_owned());
            Node { kind: Kind::Space, label, path: cwd, children, stage: None }
        })
        .collect()
}

/// An agent or subagent dir: its output.md (if written), then its subagent dirs.
fn dir_node(kind: Kind, label: &str, dir: &Path) -> Node {
    let mut children = Vec::new();
    let out = dir.join("output.md");
    if out.exists() {
        children.push(Node { kind: Kind::Output, label: "output.md".into(), path: out, children: vec![], stage: None });
    }
    let mut subs: Vec<PathBuf> = fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    subs.sort();
    for s in subs {
        children.push(dir_node(Kind::Subagent, &s.file_name().unwrap_or_default().to_string_lossy(), &s));
    }
    Node { kind, label: label.into(), path: dir.into(), children, stage: None }
}

/// Transcript path → (bytes parsed, up to the last full line; files it changed so far).
static PARSED: Mutex<BTreeMap<PathBuf, (u64, Vec<PathBuf>)>> = Mutex::new(BTreeMap::new());
/// Agent dir → (when checked; its base commit and the files `git diff --name-only` lists vs it).
type Names = (Instant, Option<(String, HashSet<PathBuf>)>);
static NAMES: Mutex<BTreeMap<PathBuf, Names>> = Mutex::new(BTreeMap::new());
/// Diff node path → (agent cwd, base commit, file path relative to the cwd).
static VIEWS: Mutex<BTreeMap<PathBuf, (PathBuf, String, String)>> = Mutex::new(BTreeMap::new());

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let o = Command::new("git").arg("-C").arg(cwd).args(args).output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// What a Diff node shows: (path relative to the agent's cwd, `git diff` of it vs the agent's base).
pub fn diff_view(key: &Path) -> Option<(String, String)> {
    let (cwd, base, rel) = VIEWS.lock().unwrap().get(key).cloned()?;
    let text = git(&cwd, &["diff", "--no-color", &base, "--", &rel]).unwrap_or_default();
    Some((rel, text))
}

/// The commit an agent started from: <dir>/base (written at launch), else where its
/// cwd's HEAD forked from the repo's HEAD.
fn base(cwd: &Path, repo: &Path, dir: &Path) -> Option<String> {
    if let Ok(b) = fs::read_to_string(dir.join("base")) {
        return Some(b.trim().to_string());
    }
    let head = git(repo, &["rev-parse", "HEAD"])?;
    Some(git(cwd, &["merge-base", "HEAD", head.trim()])?.trim().to_string())
}

/// One Diff node per file the agent's session (and its subagents) changed, inside its cwd
/// and outside its brigd dir, that `git diff` vs its base still shows (so new untracked
/// files don't). Transcripts are parsed incrementally: only bytes appended since the last
/// refresh; git is asked at most every 3s per agent.
fn diff_nodes(home: &Path, cwd: &Path, repo: &Path, dir: &Path) -> Vec<Node> {
    let Ok(session) = fs::read_to_string(dir.join("session")) else { return vec![] };
    let session = session.trim();
    let mut logs: Vec<PathBuf> = fs::read_dir(subagents_dir(home, cwd, session)).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "jsonl")).collect();
    logs.sort();
    logs.insert(0, home.join(".claude/projects").join(slug(cwd)).join(format!("{session}.jsonl")));
    let mut files: Vec<PathBuf> = Vec::new();
    let mut parsed = PARSED.lock().unwrap();
    for log in logs {
        let Ok(mut f) = fs::File::open(&log) else { continue };
        let (done, seen) = parsed.entry(log).or_default();
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < *done {
            (*done, *seen) = (0, vec![]); // rewritten: start over
        }
        let mut tail = Vec::new();
        if len > *done && f.seek(SeekFrom::Start(*done)).is_ok() && f.read_to_end(&mut tail).is_ok() {
            let end = tail.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
            add(seen, changed_files(&String::from_utf8_lossy(&tail[..end])));
            *done += end as u64;
        }
        add(&mut files, seen.clone());
    }
    drop(parsed);
    if files.is_empty() {
        return vec![];
    }
    let mut names = NAMES.lock().unwrap();
    let entry = names.entry(dir.into()).or_insert((Instant::now(), None));
    if entry.1.is_none() || entry.0.elapsed() >= Duration::from_secs(3) {
        let b = base(cwd, repo, dir);
        let listed = b.as_ref().and_then(|b| git(cwd, &["diff", "--name-only", "--relative", "-z", b]));
        *entry = (Instant::now(), b.zip(listed).map(|(b, l)| (b, l.split('\0').filter(|n| !n.is_empty()).map(|n| cwd.join(n)).collect())));
    }
    let Some((base, listed)) = entry.1.clone() else { return vec![] };
    drop(names);
    let mut views = VIEWS.lock().unwrap();
    files
        .into_iter()
        .filter(|f| !f.starts_with(dir) && listed.contains(f)) // its own output.md already shows
        .filter_map(|file| {
            let rel = file.strip_prefix(cwd).ok()?.to_string_lossy().into_owned();
            let path = dir.join(".diff").join(file.strip_prefix("/").unwrap_or(&file));
            views.insert(path.clone(), (cwd.into(), base.clone(), rel));
            let label = file.file_name()?.to_string_lossy().into_owned();
            Some(Node { kind: Kind::Diff, label, path, children: vec![], stage: None })
        })
        .collect()
}

/// Appends the files in `new` not yet in `out`.
fn add(out: &mut Vec<PathBuf>, new: Vec<PathBuf>) {
    for f in new {
        if !out.contains(&f) {
            out.push(f);
        }
    }
}

/// The files a transcript changed (Edit/MultiEdit/Write results, and Bash edits'
/// bashEditDiff.files[]), in first-change order.
pub fn changed_files(jsonl: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for v in jsonl.lines().filter(|l| l.contains("\"filePath\"")).filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        let r = &v["toolUseResult"];
        let bash = r["bashEditDiff"]["files"].as_array().into_iter().flatten().map(|f| &f["filePath"]);
        add(&mut out, bash.chain([&r["filePath"]]).filter_map(Value::as_str).map(PathBuf::from).collect());
    }
    out
}

/// The final text of a transcript: the last assistant end_turn record's text blocks.
pub fn final_text(jsonl: &str) -> Option<String> {
    let last = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["type"] == "assistant" && v["message"]["stop_reason"] == "end_turn")
        .last()?;
    let text: String = last["message"]["content"].as_array()?.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
    (!text.is_empty()).then_some(text)
}

/// Dir name for a subagent: its description as a slug, plus a short id so it stays unique and stable.
fn sub_name(id: &str, desc: &str) -> String {
    let mut s: String = desc.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s: String = s.trim_matches('-').chars().take(32).collect();
    let s = s.trim_end_matches('-');
    format!("{}-{}", if s.is_empty() { "agent" } else { s }, &id[..id.len().min(7)])
}

/// Mirrors `subagents` (claude's flat transcript dir) into nested dirs under
/// `agent_dir`, following meta.parentAgentId. Writes output.md once a subagent
/// has finished. Missing `subagents` dir = no subagents yet.
pub fn sync_subagents(subagents: &Path, agent_dir: &Path) -> io::Result<()> {
    // ponytail: only subagents/ itself; workflow agents under subagents/workflows/ are skipped.
    let mut metas: HashMap<String, (Option<String>, String)> = HashMap::new();
    for e in fs::read_dir(subagents).into_iter().flatten().flatten() {
        let f = e.file_name().to_string_lossy().into_owned();
        let Some(id) = f.strip_prefix("agent-").and_then(|f| f.strip_suffix(".meta.json")) else { continue };
        let Ok(m) = fs::read_to_string(e.path()).map(|s| serde_json::from_str::<Value>(&s).unwrap_or_default()) else { continue };
        let parent = m["parentAgentId"].as_str().map(String::from);
        metas.insert(id.into(), (parent, m["description"].as_str().unwrap_or("").into()));
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
        let dir = chain.iter().rev().fold(agent_dir.to_path_buf(), |d, i| d.join(sub_name(i, &metas[*i].1)));
        fs::create_dir_all(&dir)?;
        let jsonl = fs::read_to_string(subagents.join(format!("agent-{id}.jsonl"))).unwrap_or_default();
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

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("brigd-tree-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

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
        assert_eq!(changed_files(&j), ["/r/src/main.rs", "/r/new.md", "/r/b.sh"].map(PathBuf::from));
    }

    #[test]
    fn diff_nodes_from_git() {
        let d = tmp("diffs");
        let (home, cwd, dir) = (d.join("home"), d.join("repo"), d.join("agent"));
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(cwd.join("src")).unwrap();
        let g = |a: &[&str]| assert!(Command::new("git").arg("-C").arg(&cwd).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(a).output().unwrap().status.success());
        g(&["init", "-q"]);
        fs::write(cwd.join("src/a.rs"), "one\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "init"]);
        fs::write(dir.join("base"), git(&cwd, &["rev-parse", "HEAD"]).unwrap()).unwrap();
        fs::write(cwd.join("src/a.rs"), "one\ntwo\n").unwrap();
        g(&["commit", "-qam", "agent"]);
        fs::write(cwd.join("src/a.rs"), "one\ntwo\nthree\n").unwrap();
        fs::write(cwd.join("new.rs"), "x\n").unwrap(); // untracked
        fs::write(dir.join("session"), "s1\n").unwrap();
        let log = home.join(".claude/projects").join(slug(&cwd)).join("s1.jsonl");
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        let edit = |f: &Path| serde_json::json!({"toolUseResult": {"filePath": f}}).to_string() + "\n";
        let (a, new) = (cwd.join("src/a.rs"), cwd.join("new.rs"));
        fs::write(&log, edit(&new) + &edit(&a)[..20]).unwrap(); // second line still being written
        assert!(diff_nodes(&home, &cwd, &cwd, &dir).is_empty());
        fs::write(&log, edit(&new) + &edit(&a) + &edit(&dir.join("output.md"))).unwrap();
        let n = diff_nodes(&home, &cwd, &cwd, &dir);
        assert_eq!((n.len(), n[0].label.as_str(), n[0].kind.clone()), (1, "a.rs", Kind::Diff));
        let (rel, text) = diff_view(&n[0].path).unwrap();
        assert_eq!(rel, "src/a.rs");
        assert!(text.contains("+two\n") && text.contains("+three\n"), "{text}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn slug_rule() {
        assert_eq!(slug(Path::new("/Users/mark.verma/.brigd/x")), "-Users-mark-verma--brigd-x");
    }

    #[test]
    fn final_text_picks_last_end_turn() {
        let tool = r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"text","text":"no"}]}}"#;
        let j = [end_turn("first"), tool.into(), end_turn("last"), r#"{"type":"user"}"#.into()].join("\n");
        assert_eq!(final_text(&j).as_deref(), Some("last"));
        assert_eq!(final_text(tool), None);
    }

    #[test]
    fn sync_nests_subagents() {
        let d = tmp("sync");
        let (subs, agent) = (d.join("subagents"), d.join("agent"));
        fs::create_dir_all(&subs).unwrap();
        let meta = |id: &str, m: Value| fs::write(subs.join(format!("agent-{id}.meta.json")), m.to_string()).unwrap();
        meta("aaaaaaaa1", serde_json::json!({"description": "Find the bug!", "spawnDepth": 0}));
        meta("bbbbbbbb2", serde_json::json!({"description": "grep", "spawnDepth": 1, "parentAgentId": "aaaaaaaa1"}));
        meta("ccccccccc", serde_json::json!({"description": "", "spawnDepth": 0}));
        fs::write(subs.join("agent-aaaaaaaa1.jsonl"), end_turn("top done")).unwrap();
        fs::write(subs.join("agent-bbbbbbbb2.jsonl"), end_turn("child done")).unwrap();
        fs::write(subs.join("agent-ccccccccc.jsonl"), r#"{"type":"user"}"#).unwrap();
        sync_subagents(&subs, &agent).unwrap();
        let top = agent.join("find-the-bug-aaaaaaa");
        assert_eq!(fs::read_to_string(top.join("output.md")).unwrap(), "top done");
        assert_eq!(fs::read_to_string(top.join("grep-bbbbbbb/output.md")).unwrap(), "child done");
        assert!(agent.join("agent-ccccccc").is_dir());
        assert!(!agent.join("agent-ccccccc/output.md").exists()); // still running
        sync_subagents(&d.join("missing"), &agent).unwrap();
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn build_groups_by_space() {
        let root = tmp("build");
        let t = root.join("threads/t1");
        fs::create_dir_all(t.join("a/sub-1234567/deep-7654321")).unwrap();
        fs::write(t.join("a/output.md"), "x").unwrap();
        fs::write(t.join("a/sub-1234567/output.md"), "y").unwrap();
        let flow = serde_json::json!({"goal":"g","stages":[[{"name":"a","worktree":null}],[{"name":"b","worktree":"fix"}]]});
        fs::write(t.join("flowmap.json"), flow.to_string()).unwrap();
        fs::write(t.join("state.json"), r#"{"repo":"/src/myrepo","agents":{"a":{"status":"done"},"b":{"status":"running"}}}"#).unwrap();
        let t2 = root.join("threads/t2"); // c not started yet: hidden, thread shows its FLOWPLAN under the repo
        fs::create_dir_all(t2.join("c")).unwrap();
        fs::write(t2.join("flowmap.json"), serde_json::json!({"stages":[[{"name":"c","worktree":"fix"}]]}).to_string()).unwrap();
        fs::write(t2.join("state.json"), r#"{"repo":"/src/myrepo","agents":{}}"#).unwrap();
        fs::write(t2.join("FLOWPLAN"), "plan").unwrap();
        fs::create_dir_all(root.join("threads/junk")).unwrap(); // no flowmap.json: skipped
        let tree = build(&root);
        let labels: Vec<&str> = tree.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(labels, ["myrepo", "fix"]);
        assert!(tree.iter().all(|n| n.kind == Kind::Space));
        let a = &tree[0].children[0].children[0];
        assert_eq!((tree[0].children[0].label.as_str(), a.label.as_str(), a.stage), ("t1", "a", Some(0)));
        assert_eq!(a.children[0].kind, Kind::Output);
        let sub = &a.children[1];
        assert_eq!((sub.kind.clone(), sub.label.as_str()), (Kind::Subagent, "sub-1234567"));
        assert_eq!(sub.children[0].kind, Kind::Output);
        assert_eq!(sub.children[1].label, "deep-7654321");
        let t2n = &tree[0].children[1];
        assert_eq!((t2n.label.as_str(), t2n.children.len(), t2n.children[0].label.as_str()), ("t2", 1, "FLOWPLAN"));
        assert_eq!(tree[1].children.len(), 1); // t2's unstarted c is not under fix
        let b = &tree[1].children[0].children[0];
        assert_eq!((b.label.as_str(), b.children.len(), b.stage), ("b", 0, Some(1)));
        assert_eq!(tree[1].path, root.join("worktrees/t1/fix"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_places_strays() {
        let root = tmp("strays");
        let t = root.join("threads/t1");
        let stray = |n: &str, cwd: &str| {
            fs::create_dir_all(t.join(STRAY).join(n)).unwrap();
            fs::write(t.join(STRAY).join(n).join("cwd"), cwd).unwrap();
            fs::write(t.join(STRAY).join(n).join("session"), "s").unwrap();
        };
        stray("stray-1", "/src/myrepo/sub\n");
        stray("stray-2", "/elsewhere/x");
        fs::create_dir_all(t.join("a")).unwrap();
        fs::write(t.join("FLOWPLAN"), "plan").unwrap();
        fs::write(t.join("flowmap.json"), serde_json::json!({"stages":[[{"name":"a","worktree":null}]]}).to_string()).unwrap();
        fs::write(t.join("state.json"), r#"{"repo":"/src/myrepo","agents":{"a":{"status":"done"}}}"#).unwrap();
        let tree = build(&root);
        let space = |l: &str| &tree.iter().find(|s| s.label == l).unwrap().children[0];
        let labels = |n: &Node| n.children.iter().map(|c| c.label.clone()).collect::<Vec<_>>();
        let th = space("myrepo");
        assert_eq!(labels(th), ["FLOWPLAN", STRAY, "a"]);
        assert_eq!((th.children[1].kind.clone(), labels(&th.children[1])), (Kind::Folder, vec!["stray-1".to_string()]));
        let s1 = &th.children[1].children[0];
        assert_eq!((s1.kind.clone(), s1.stage, agent_id(&s1.path, &s1.label)), (Kind::Agent, None, format!("{STRAY}/stray-1")));
        assert_eq!(agent_id(&th.children[2].path, "a"), "a");
        let x = space("x"); // cwd outside every space of the thread: a space of its own
        assert_eq!((labels(x), labels(&x.children[1])), (vec!["FLOWPLAN".to_string(), STRAY.into()], vec!["stray-2".to_string()]));
        let _ = fs::remove_dir_all(&root);
    }
}
