//! The sidebar tree, read from disk: space (repo basename) → thread → agent →
//! [output.md, flow.md, subagent dirs → output.md …, changed files].

use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Space,
    Thread,
    Agent,
    Subagent,
    Output,
    /// A file the agent changed; opens as a diff (see `diff_view`).
    Diff,
    /// The thread's TERMS folder.
    Folder,
    /// A live brigd terminal (shell tab) whose $BRIGD_THREAD is the thread; listed by the TUI, not on disk.
    Terminal,
    /// The TUI's pinned top row: the master session (runner::MASTER), not on disk.
    Master,
}

/// <thread>/stray agents/<name>/{cwd,session,base,pid,term,status}: claude sessions typed in a brigd terminal.
pub const STRAY: &str = "stray agents";

/// The folder of a thread's live terminals (synthetic path <thread dir>/.terminal).
pub const TERMS: &str = "terminal";

/// The dirs of a thread's strays (<thread dir>/stray agents/*), recorded on disk but not in the tree.
pub(crate) fn stray_dirs(thread_dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(thread_dir.join(STRAY))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    v.sort();
    v
}

/// An agent's registry/state name: its label, or "stray agents/<label>" for a stray.
pub fn agent_id(path: &Path, label: &str) -> String {
    if path.parent().is_some_and(|p| p.ends_with(STRAY)) {
        format!("{STRAY}/{label}")
    } else {
        label.into()
    }
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
    /// Agent: the thread worktree key it runs in; None for the repo's main checkout.
    pub worktree: Option<String>,
}

impl Node {
    /// A node without children, stage or worktree.
    pub(crate) fn new(kind: Kind, label: impl Into<String>, path: PathBuf) -> Node {
        Node {
            kind,
            label: label.into(),
            path,
            children: vec![],
            stage: None,
            worktree: None,
        }
    }
}

/// The directory a space's agents run in: the repo for key "", else its worktree.
pub fn space_cwd(root: &Path, repo: &Path, thread: &str, key: &str) -> PathBuf {
    if key.is_empty() {
        repo.into()
    } else {
        root.join("worktrees").join(thread).join(key)
    }
}

/// Builds the sidebar from `root` (~/.brigd): one space per repo, each thread under its
/// repo once, holding all its agents (each tagged with the worktree it runs in).
pub fn build(root: &Path) -> Vec<Node> {
    let home = crate::store::home();
    let mut spaces: BTreeMap<PathBuf, Vec<Node>> = BTreeMap::new();
    let mut threads: Vec<PathBuf> = fs::read_dir(root.join("threads"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    threads.sort();
    for tdir in threads {
        if let Some((repo, thread)) = read_thread(root, &home, &tdir) {
            spaces.entry(repo).or_default().push(thread);
        }
    }
    spaces
        .into_iter()
        .map(|(cwd, children)| {
            let label = cwd
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| cwd.to_string_lossy().into_owned());
            Node {
                children,
                ..Node::new(Kind::Space, label, cwd)
            }
        })
        .collect()
}

/// A thread's repo and node: FLOWPLAN first, then its launched agents with their diffs.
/// None unless both its flowmap.json and state.json parse.
fn read_thread(root: &Path, home: &Path, tdir: &Path) -> Option<(PathBuf, Node)> {
    let read = |f: &str| {
        fs::read_to_string(tdir.join(f))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    };
    let (Some(flow), Some(state)) = (read("flowmap.json"), read("state.json")) else {
        return None;
    };
    let thread = tdir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let repo = PathBuf::from(state["repo"].as_str().unwrap_or_default());
    let mut agents = Vec::new();
    for (si, a) in flow["stages"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .flat_map(|(i, s)| s.as_array().into_iter().flatten().map(move |a| (i, a)))
    {
        // Only agents that have been launched: set_agent puts them in state.json's "agents".
        let Some(name) = a["name"]
            .as_str()
            .filter(|n| !state["agents"][*n].is_null())
        else {
            continue;
        };
        let key = a["worktree"].as_str().unwrap_or("");
        let cwd = space_cwd(root, &repo, &thread, key);
        let mut agent = Node {
            stage: Some(si),
            worktree: Some(key.to_string()).filter(|k| !k.is_empty()),
            ..dir_node(Kind::Agent, name, &tdir.join(name))
        };
        agent.children.extend(
            crate::diffs::diff_paths(home, &cwd, &repo, &agent.path)
                .into_iter()
                .map(|p| {
                    let label = p
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    Node::new(Kind::Diff, label, p)
                }),
        );
        agents.push(agent);
    }
    // FLOWPLAN (the plan + live run log) is the thread's first child. Strays stay on disk only (stray agents/).
    let plan = tdir.join("FLOWPLAN");
    if plan.exists() {
        agents.insert(0, Node::new(Kind::Output, "FLOWPLAN", plan));
    }
    let node = Node {
        children: agents,
        ..Node::new(Kind::Thread, thread, tdir.into())
    };
    Some((repo, node))
}

/// An agent or subagent dir: its output.md and flow.md (if written), then its subagent dirs.
fn dir_node(kind: Kind, label: &str, dir: &Path) -> Node {
    let mut children = Vec::new();
    for name in ["output.md", "flow.md"] {
        let out = dir.join(name);
        if out.exists() {
            children.push(Node::new(Kind::Output, name, out));
        }
    }
    // dot dirs (.diff, the frozen diffs) are brigd's own, not subagents
    let mut subs: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .collect();
    subs.sort();
    for s in subs {
        children.push(dir_node(
            Kind::Subagent,
            &s.file_name().unwrap_or_default().to_string_lossy(),
            &s,
        ));
    }
    Node {
        children,
        ..Node::new(kind, label, dir.into())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("brigd-tree-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn build_groups_by_repo() {
        let root = tmp("build");
        let t = root.join("threads/t1");
        fs::create_dir_all(t.join("a/sub-1234567/deep-7654321")).unwrap();
        fs::write(t.join("a/output.md"), "x").unwrap();
        fs::write(t.join("a/flow.md"), "f").unwrap();
        fs::write(t.join("a/sub-1234567/output.md"), "y").unwrap();
        let flow = serde_json::json!({"goal":"g","stages":[[{"name":"a","worktree":null}],[{"name":"b","worktree":"fix"}]]});
        fs::write(t.join("flowmap.json"), flow.to_string()).unwrap();
        fs::write(
            t.join("state.json"),
            r#"{"repo":"/src/myrepo","agents":{"a":{"status":"done"},"b":{"status":"running"}}}"#,
        )
        .unwrap();
        let t2 = root.join("threads/t2"); // c not started yet: hidden, thread shows its FLOWPLAN under the repo
        fs::create_dir_all(t2.join("c")).unwrap();
        fs::write(
            t2.join("flowmap.json"),
            serde_json::json!({"stages":[[{"name":"c","worktree":"fix"}]]}).to_string(),
        )
        .unwrap();
        fs::write(
            t2.join("state.json"),
            r#"{"repo":"/src/myrepo","agents":{}}"#,
        )
        .unwrap();
        fs::write(t2.join("FLOWPLAN"), "plan").unwrap();
        fs::create_dir_all(root.join("threads/junk")).unwrap(); // no flowmap.json: skipped
        let tree = build(&root);
        // Both threads, and t1's agents in the repo and in worktree "fix", sit under one space.
        let labels: Vec<&str> = tree.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(labels, ["myrepo"]);
        assert_eq!(
            (tree[0].kind.clone(), tree[0].path.clone()),
            (Kind::Space, PathBuf::from("/src/myrepo"))
        );
        let t1n = &tree[0].children[0];
        let a = &t1n.children[0];
        assert_eq!(
            (
                t1n.label.as_str(),
                t1n.children.len(),
                a.label.as_str(),
                a.stage,
                a.worktree.clone()
            ),
            ("t1", 2, "a", Some(0), None)
        );
        let outs: Vec<(Kind, &str)> = a.children[..2]
            .iter()
            .map(|c| (c.kind.clone(), c.label.as_str()))
            .collect();
        assert_eq!(
            outs,
            [(Kind::Output, "output.md"), (Kind::Output, "flow.md")]
        );
        let sub = &a.children[2];
        assert_eq!(
            (sub.kind.clone(), sub.label.as_str()),
            (Kind::Subagent, "sub-1234567")
        );
        assert_eq!(sub.children[0].kind, Kind::Output);
        assert_eq!(sub.children[1].label, "deep-7654321"); // no flow.md: no node
        let b = &t1n.children[1];
        assert_eq!(
            (
                b.label.as_str(),
                b.children.len(),
                b.stage,
                b.worktree.as_deref()
            ),
            ("b", 0, Some(1), Some("fix"))
        );
        let t2n = &tree[0].children[1]; // t2's unstarted c is hidden
        assert_eq!(
            (
                t2n.label.as_str(),
                t2n.children.len(),
                t2n.children[0].label.as_str()
            ),
            ("t2", 1, "FLOWPLAN")
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_hides_strays() {
        let root = tmp("strays");
        let t = root.join("threads/t1");
        fs::create_dir_all(t.join(STRAY).join("stray-1")).unwrap();
        fs::write(
            t.join(STRAY).join("stray-1").join("cwd"),
            "/src/myrepo/sub\n",
        )
        .unwrap();
        fs::create_dir_all(t.join("a")).unwrap();
        fs::write(t.join("FLOWPLAN"), "plan").unwrap();
        fs::write(
            t.join("flowmap.json"),
            serde_json::json!({"stages":[[{"name":"a","worktree":null}]]}).to_string(),
        )
        .unwrap();
        fs::write(
            t.join("state.json"),
            r#"{"repo":"/src/myrepo","agents":{"a":{"status":"done"}}}"#,
        )
        .unwrap();
        let tree = build(&root);
        assert_eq!(tree.len(), 1);
        let th = &tree[0].children[0];
        assert_eq!(
            th.children
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>(),
            ["FLOWPLAN", "a"]
        ); // the stray is on disk, not in the tree
        assert_eq!(agent_id(&th.children[1].path, "a"), "a");
        assert_eq!(
            agent_id(&t.join(STRAY).join("stray-1"), "stray-1"),
            format!("{STRAY}/stray-1")
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn dot_dirs_are_not_subagents() {
        let d = tmp("dotdirs");
        fs::create_dir_all(d.join(".diff/x")).unwrap();
        fs::create_dir_all(d.join("sub-1234567")).unwrap();
        let n = dir_node(Kind::Agent, "a", &d);
        assert_eq!(
            n.children
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>(),
            ["sub-1234567"]
        );
        let _ = fs::remove_dir_all(&d);
    }
}
