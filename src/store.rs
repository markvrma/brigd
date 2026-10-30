//! Thread data and its on-disk layout: ~/.brigd/threads/<thread>/{flowmap.json,state.json,FLOWPLAN}
//! and <agent>/ dirs. Reading, writing, creating, archiving and deleting threads; no processes.

use crate::Res;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, io};

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Agent {
    pub(crate) name: String,
    /// The coding agent (agents::AGENTS) that runs it; absent in older flowmaps = claude.
    #[serde(default = "default_agent")]
    pub(crate) agent: String,
    pub(crate) model: String,
    pub(crate) effort: String,
    #[serde(default)]
    pub(crate) worktree: Option<String>,
    pub(crate) task: String,
    #[serde(default)]
    pub(crate) files: Vec<String>,
    #[serde(default = "default_mode")]
    pub(crate) mode: String,
}

fn default_mode() -> String {
    "acceptEdits".into()
}

fn default_agent() -> String {
    "claude".into()
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Flow {
    pub(crate) goal: String,
    pub(crate) stages: Vec<Vec<Agent>>,
}

/// A thread's or agent's status: what state.json stores (lowercase), what the hooks write to an
/// agent's status file, and the TUI's own off / `$` (a plain terminal).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq, Hash)]
#[serde(from = "String", into = "String")]
pub(crate) enum Status {
    /// Nothing yet ("" in older state.json files).
    #[default]
    Pending,
    /// Thread: created, waiting to be run.
    Planned,
    /// Thread: handed to the open TUI (`brigd resume --bg`).
    Queued,
    Running,
    /// Thread: set by hand (the brigd-resume skill) so it can be resumed.
    Stopped,
    Done,
    Failed,
    /// Hook status: claude has not reported yet.
    Starting,
    /// Hook status: claude is working.
    Working,
    /// Hook status: claude finished its turn.
    Idle,
    /// Hook status: claude waits for the user.
    Blocked,
    /// TUI: the agent's process is gone and state.json says neither done nor failed.
    Off,
    /// TUI: a terminal tab with no claude running in it.
    Term,
    /// Any other value (a hand-edited state.json): shown and written back verbatim, never acted on.
    Other(String),
}

impl Status {
    /// A status from its stored or hook-written text ("" is Pending).
    pub(crate) fn parse(s: &str) -> Status {
        match s {
            "" | "pending" => Status::Pending,
            "planned" => Status::Planned,
            "queued" => Status::Queued,
            "running" => Status::Running,
            "stopped" => Status::Stopped,
            "done" => Status::Done,
            "failed" => Status::Failed,
            "starting" => Status::Starting,
            "working" => Status::Working,
            "idle" => Status::Idle,
            "blocked" => Status::Blocked,
            "off" => Status::Off,
            "$" => Status::Term,
            other => Status::Other(other.into()),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        match self {
            Status::Pending => "pending",
            Status::Planned => "planned",
            Status::Queued => "queued",
            Status::Running => "running",
            Status::Stopped => "stopped",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Starting => "starting",
            Status::Working => "working",
            Status::Idle => "idle",
            Status::Blocked => "blocked",
            Status::Off => "off",
            Status::Term => "$",
            Status::Other(s) => s,
        }
    }
}

impl From<String> for Status {
    fn from(s: String) -> Status {
        Status::parse(&s)
    }
}

impl From<Status> for String {
    fn from(s: Status) -> String {
        s.as_str().into()
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.pad(self.as_str())
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub(crate) struct AgentState {
    #[serde(default)]
    pub(crate) status: Status,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct State {
    pub(crate) thread: String,
    pub(crate) repo: String,
    #[serde(default)]
    pub(crate) status: Status,
    pub(crate) stage: usize,
    pub(crate) updated: u64,
    pub(crate) agents: BTreeMap<String, AgentState>,
}

/// $HOME ("" if unset).
pub(crate) fn home() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_default())
}

/// ~/.brigd
pub(crate) fn root() -> PathBuf {
    home().join(".brigd")
}

pub(crate) fn thread_dir(name: &str) -> PathBuf {
    root().join("threads").join(name)
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A thread's flowmap.json and state.json.
pub(crate) fn load(name: &str) -> Res<(Flow, State)> {
    let dir = thread_dir(name);
    let read =
        |f: &str| fs::read_to_string(dir.join(f)).map_err(|e| format!("thread {name}: {f}: {e}"));
    Ok((
        serde_json::from_str(&read("flowmap.json")?)?,
        serde_json::from_str(&read("state.json")?)?,
    ))
}

/// Writes via temp file + rename so a crash never leaves a half-written file.
pub(crate) fn write_json(path: &Path, v: &impl Serialize) -> Res<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_string_pretty(v)?)?;
    Ok(fs::rename(tmp, path)?)
}

pub(crate) fn update(state: &Mutex<State>, f: impl FnOnce(&mut State)) -> Res<()> {
    let mut s = state.lock().unwrap();
    f(&mut s);
    s.updated = now();
    write_json(&thread_dir(&s.thread).join("state.json"), &*s)
}

pub(crate) fn set_agent(state: &Mutex<State>, agent: &str, status: Status) -> Res<()> {
    update(state, |s| {
        s.agents.entry(agent.into()).or_default().status = status
    })
}

/// Appends to ~/.brigd/threads/<thread>/FLOWPLAN, the plan followed by the live run log.
/// execute never prints: the TUI owns the screen and shows this file.
/// Log lines go under the plan's last line, LOG_MARK, which the TUI's FLOWPLAN view splits on.
pub(crate) fn log(thread: &str, msg: &str) {
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(thread_dir(thread).join("FLOWPLAN"))
    {
        let _ = writeln!(f, "   {msg}");
    }
}

/// Subcommand names (and the TUI's master row key), so never thread names.
pub(crate) const RESERVED: &[&str] = &[
    "ls",
    "status",
    "resume",
    "install-skill",
    "help",
    "stray",
    "master",
];
// Keeps "<thread>/<agent>" labels short enough for a sidebar (with flow's AGENT_MAX).
pub(crate) const THREAD_MAX: usize = 12;

/// A thread, agent or worktree name: [a-z][a-z0-9_-], at most `max` bytes.
pub(crate) fn valid_name(s: &str, max: usize) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= max
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

pub(crate) fn check_new_thread(name: &str) -> Res<()> {
    if RESERVED.contains(&name) || !valid_name(name, THREAD_MAX) {
        return Err(format!(
            "bad thread name {name:?}: [a-z][a-z0-9_-], max {THREAD_MAX}, not one of {RESERVED:?}"
        )
        .into());
    }
    if thread_dir(name).exists() {
        return Err(format!(
            "thread {name} exists: `brigd resume {name}`, or delete {} to reuse the name",
            thread_dir(name).display()
        )
        .into());
    }
    Ok(())
}

/// Writes a new thread's dir (it may already exist, reserved by the TUI), state.json with
/// status "planned" and flowmap.json. Callers write FLOWPLAN themselves.
pub(crate) fn create_thread(root: &Path, name: &str, repo: &str, flow: &Flow) -> Res<State> {
    let dir = root.join("threads").join(name);
    fs::create_dir_all(&dir)?;
    let state = State {
        thread: name.into(),
        repo: repo.into(),
        status: Status::Planned,
        updated: now(),
        ..Default::default()
    };
    write_json(&dir.join("state.json"), &state)?;
    write_json(&dir.join("flowmap.json"), flow)?;
    Ok(state)
}

/// A thread with no activity for this long (secs) is auto-archived by the TUI.
pub(crate) const ARCHIVE_AFTER: u64 = 7 * 86400;

/// mtime of `meta` in unix secs; a missing or unreadable one reads as now, so it counts as active.
fn mtime_secs(meta: io::Result<fs::Metadata>) -> u64 {
    meta.and_then(|m| m.modified())
        .map(|t| t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()))
        .unwrap_or_else(|_| now())
}

/// Raises `max` to the newest mtime of the files under `dir`. Directory mtimes and brigd's
/// `.diff/` caches (rebuilt lazily, so they say nothing about use) are ignored, symlinks are
/// never followed, and any read error but a missing dir counts as active now.
fn newest_file(dir: &Path, max: &mut u64) {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return,
        Err(_) => return *max = now(),
    };
    for e in rd {
        let Ok(e) = e else {
            *max = now();
            continue;
        };
        match e.file_type() {
            Ok(t) if t.is_dir() => {
                if e.file_name() != ".diff" {
                    newest_file(&e.path(), max)
                }
            }
            Ok(t) if t.is_file() => *max = (*max).max(mtime_secs(e.metadata())),
            Ok(_) => {}
            Err(_) => *max = now(),
        }
    }
}

/// When a thread's own files last changed (unix secs; 0 if it has none). A future mtime is
/// "now or later", so `now().saturating_sub` of it reads as just active.
pub(crate) fn last_active(tdir: &Path) -> u64 {
    let mut max = 0;
    newest_file(tdir, &mut max);
    max
}

/// The newest mtime of the claude transcripts (and subagent transcripts) of the thread's agents
/// and strays, 0 if none: resuming a session outside brigd writes there, not under the thread.
pub(crate) fn transcript_active(root: &Path, home: &Path, tdir: &Path) -> u64 {
    let thread = tdir.file_name().unwrap_or_default().to_string_lossy();
    let json = |f: &str| {
        fs::read_to_string(tdir.join(f))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .unwrap_or_default()
    };
    let (flow, state) = (json("flowmap.json"), json("state.json"));
    let repo = PathBuf::from(state["repo"].as_str().unwrap_or_default());
    let mut runs: Vec<(PathBuf, PathBuf)> = flow["stages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|s| s.as_array().into_iter().flatten())
        .filter_map(|a| {
            let key = a["worktree"].as_str().unwrap_or("");
            Some((
                tdir.join(a["name"].as_str()?),
                crate::tree::space_cwd(root, &repo, &thread, key),
            ))
        })
        .collect();
    runs.extend(crate::tree::stray_dirs(tdir).into_iter().filter_map(|d| {
        let cwd = fs::read_to_string(d.join("cwd")).ok()?;
        Some((d, PathBuf::from(cwd.trim())))
    }));
    let mut max = 0;
    for (dir, cwd) in runs {
        let Ok(session) = fs::read_to_string(dir.join("session")) else {
            continue;
        };
        let session = session.trim();
        if let Ok(m) = fs::metadata(crate::transcript::transcript_path(home, &cwd, session)) {
            max = max.max(mtime_secs(Ok(m)));
        }
        newest_file(
            &crate::transcript::subagents_dir(home, &cwd, session),
            &mut max,
        );
    }
    max
}

/// <root>/archive/threads/<thread>: where archive_thread moves a thread.
pub(crate) fn archive_dir(root: &Path, thread: &str) -> PathBuf {
    root.join("archive/threads").join(thread)
}

/// Moves <root>/threads/<thread> to archive_dir, unchanged; refuses if that exists. Its git
/// worktrees stay in <root>/worktrees (moving them by hand would break git's links to them).
pub(crate) fn archive_thread(root: &Path, thread: &str) -> Res<PathBuf> {
    let to = archive_dir(root, thread);
    if to.exists() {
        return Err(format!("{} already exists", to.display()).into());
    }
    fs::create_dir_all(to.parent().unwrap_or(root))?;
    fs::rename(root.join("threads").join(thread), &to)?;
    Ok(to)
}

/// Removes `thread` for good: its git worktrees (in its state.json repo) and their
/// brigd/<thread>/* branches, then <root>/threads/<thread> and <root>/worktrees/<thread>.
pub(crate) fn delete_thread(root: &Path, thread: &str) -> Res<()> {
    let (tdir, wdir) = (
        root.join("threads").join(thread),
        root.join("worktrees").join(thread),
    );
    let state: serde_json::Value = fs::read_to_string(tdir.join("state.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let repo = state["repo"]
        .as_str()
        .map(PathBuf::from)
        .filter(|r| r.exists());
    let git = |args: &[&str]| {
        repo.as_ref().and_then(|r| {
            Command::new("git")
                .arg("-C")
                .arg(r)
                .args(args)
                .output()
                .ok()
        })
    };
    for wt in fs::read_dir(&wdir).into_iter().flatten().flatten() {
        let _ = git(&[
            "worktree",
            "remove",
            "--force",
            &wt.path().to_string_lossy(),
        ]);
    }
    for d in [&tdir, &wdir] {
        match fs::remove_dir_all(d) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(format!("{}: {e}", d.display()).into())
            }
            _ => {}
        }
    }
    let _ = git(&["worktree", "prune"]); // forgets any worktree `remove` refused
    let refs = git(&[
        "for-each-ref",
        "--format=%(refname:short)",
        &format!("refs/heads/brigd/{thread}/"),
    ])
    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    .unwrap_or_default();
    let mut kept = vec![];
    for b in refs.lines() {
        if !git(&["branch", "-D", b]).is_some_and(|o| o.status.success()) {
            kept.push(b);
        }
    }
    if !kept.is_empty() {
        return Err(format!("could not delete branch(es) {}", kept.join(", ")).into());
    }
    Ok(())
}

pub(crate) fn output_path(thread: &str, agent: &str) -> PathBuf {
    thread_dir(thread).join(agent).join("output.md")
}

pub(crate) fn ago(t: u64) -> String {
    let d = now().saturating_sub(t);
    match d {
        0..=59 => format!("{d}s ago"),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::tests::agent;

    #[test]
    fn archive_moves_thread_and_refuses_overwrite() {
        let root = env::temp_dir().join(format!("brigd-archive-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("threads/th/a")).unwrap();
        fs::write(root.join("threads/th/a/output.md"), "x").unwrap();
        let to = archive_thread(&root, "th").unwrap();
        assert_eq!(to, root.join("archive/threads/th"));
        assert!(!root.join("threads/th").exists() && to.join("a/output.md").exists());
        fs::create_dir_all(root.join("threads/th")).unwrap();
        assert!(archive_thread(&root, "th")
            .unwrap_err()
            .to_string()
            .contains("already exists"));
        assert!(root.join("threads/th").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn last_active_ignores_dirs_and_diff_caches() {
        let root = env::temp_dir().join(format!("brigd-active-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let t = root.join("th");
        fs::create_dir_all(t.join("a/.diff/x")).unwrap();
        let age = |p: PathBuf, secs: u64| {
            let f = fs::OpenOptions::new().write(true).open(p).unwrap();
            f.set_modified(SystemTime::now() - std::time::Duration::from_secs(secs))
                .unwrap();
        };
        let old = 8 * 86400;
        for f in ["state.json", "a/output.md", "a/deep.md"] {
            fs::write(t.join(f), "x").unwrap();
            age(t.join(f), old);
        }
        // A fresh file in .diff/ (and the fresh dirs holding it) is not activity.
        fs::write(t.join("a/.diff/x/f"), "x").unwrap();
        let stale = |t: &Path| now().saturating_sub(last_active(t)) >= ARCHIVE_AFTER;
        assert!(stale(&t));
        // A fresh file deep in the thread is.
        fs::create_dir_all(t.join("a/sub/deeper")).unwrap();
        fs::write(t.join("a/sub/deeper/f"), "x").unwrap();
        assert!(!stale(&t));
        age(t.join("a/sub/deeper/f"), old);
        assert!(stale(&t));
        // A future mtime counts as active.
        let f = fs::OpenOptions::new()
            .write(true)
            .open(t.join("state.json"))
            .unwrap();
        f.set_modified(SystemTime::now() + std::time::Duration::from_secs(3600))
            .unwrap();
        assert!(!stale(&t));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_removes_worktrees_branches_and_dirs() {
        let root = env::temp_dir().join(format!("brigd-delete-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let git = |a: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(a)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&["branch", "brigd/other/k"]);
        for k in ["k1", "k2"] {
            let wt = root.join("worktrees/th").join(k);
            assert!(git(&[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("brigd/th/{k}"),
                &wt.to_string_lossy()
            ])
            .status
            .success());
        }
        fs::create_dir_all(root.join("threads/th")).unwrap();
        fs::write(
            root.join("threads/th/state.json"),
            format!(r#"{{"repo":{:?}}}"#, repo.to_string_lossy()),
        )
        .unwrap();
        delete_thread(&root, "th").unwrap();
        assert!(!root.join("threads/th").exists() && !root.join("worktrees/th").exists());
        let out = |a: &[&str]| String::from_utf8_lossy(&git(a).stdout).into_owned();
        assert_eq!(out(&["worktree", "list"]).lines().count(), 1);
        assert_eq!(
            out(&["branch", "--list", "brigd/*", "--format=%(refname:short)"]).trim(),
            "brigd/other/k"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn create_thread_in_repo() {
        let root = env::temp_dir().join(format!("brigd-create-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("threads/th")).unwrap(); // reserved by the TUI
        let flow = Flow {
            goal: "g".into(),
            stages: vec![vec![agent("a", None, "default")]],
        };
        create_thread(&root, "th", "/some/repo", &flow).unwrap();
        let st: State =
            serde_json::from_str(&fs::read_to_string(root.join("threads/th/state.json")).unwrap())
                .unwrap();
        assert_eq!(
            (st.repo.as_str(), st.status),
            ("/some/repo", Status::Planned)
        );
        assert!(root.join("threads/th/flowmap.json").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn thread_names() {
        assert!(!valid_name("a-very-long-thread", THREAD_MAX));
        assert!(RESERVED.contains(&"ls"));
        assert!(check_new_thread("master").is_err());
    }

    #[test]
    fn status_wire_format() {
        let st: State = serde_json::from_str(
            r#"{"thread":"t","repo":"r","status":"stopped","stage":0,"updated":0,
                "agents":{"a":{"status":""},"b":{"status":"weird"},"c":{}}}"#,
        )
        .unwrap();
        assert_eq!(st.status, Status::Stopped);
        let a: Vec<Status> = st.agents.values().map(|a| a.status.clone()).collect();
        assert_eq!(
            a,
            [
                Status::Pending,
                Status::Other("weird".into()),
                Status::Pending
            ]
        );
        let json = serde_json::to_string(&st).unwrap();
        assert!(json.contains(r#""status":"stopped""#) && json.contains(r#""status":"pending""#));
        assert!(json.contains(r#""status":"weird""#)); // kept verbatim
        assert_eq!(Status::parse("$"), Status::Term);
        assert_eq!(format!("{:<8}|", Status::Done), "done    |");
    }
}
