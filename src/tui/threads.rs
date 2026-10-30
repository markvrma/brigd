//! Thread and agent actions: pause/resume, terminate, archive, delete, the right-click menu, claiming queued flows, and stray/terminal rows in the tree.

use super::newt::Bg;
use super::App;
use crate::runner::{self, LiveAgent};
use crate::store::Status;
use crate::tree::{self, Kind, Node};
use ratatui::layout::Rect;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

/// The right-click popup: an agent row's Terminate, a thread row's Delete / Archive.
/// Item `i` fills line `rect.y + i`; `sel` is the one Enter picks.
pub(super) struct Menu {
    pub(super) rect: Rect,
    pub(super) items: Vec<Act>,
    pub(super) sel: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Act {
    Terminate { thread: String, agent: String },
    Delete(String),
    Archive(String),
}

impl Act {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Act::Terminate { .. } => " Terminate ",
            Act::Delete(_) => " Delete    ",
            Act::Archive(_) => " Archive   ",
        }
    }
}

/// A copy of the live-agent registry: (thread, agent) -> LiveAgent.
pub(super) type Snapshot = BTreeMap<runner::Key, Arc<LiveAgent>>;

/// A copy of the live-agent registry, so the lock is never held across PTY calls.
pub(super) fn registry_snapshot() -> Snapshot {
    runner::registry().lock().unwrap().clone()
}

pub(super) fn live_agents() -> Vec<Arc<LiveAgent>> {
    registry_snapshot()
        .into_values()
        .filter(|a| a.alive())
        .collect()
}

/// While the process lives its hook status wins (a done agent the user types into is live
/// again): running / blocked / idle. Once it exits: done / failed from state.json, else off.
pub(super) fn agent_glyph_status(live: Option<Status>, saved: Status) -> Status {
    match live {
        Some(Status::Blocked) => Status::Blocked,
        // Non-claude agents have no hook status; supervise saves Blocked after a silent stall.
        Some(Status::Idle) if saved == Status::Blocked => Status::Blocked,
        Some(Status::Idle) => Status::Idle,
        Some(_) => Status::Running,
        None if saved == Status::Done => Status::Done,
        None if saved == Status::Failed => Status::Failed,
        None => Status::Off,
    }
}

/// Lists each thread's live terminals (`terms`: (thread, shell name)) in a TERMS folder, after its
/// FLOWPLAN if it has one, else first.
pub(super) fn add_terminals(tree: &mut [Node], terms: &[(String, String)]) {
    for t in tree.iter_mut().flat_map(|s| &mut s.children) {
        let mut names: Vec<&str> = terms
            .iter()
            .filter(|(th, _)| *th == t.label)
            .map(|(_, n)| n.as_str())
            .collect();
        if names.is_empty() {
            continue;
        }
        names.sort();
        let dir = t.path.join(".terminal");
        let node = |kind, label: &str, path, children| Node {
            kind,
            label: label.into(),
            path,
            children,
            stage: None,
            worktree: None,
        };
        let kids = names
            .into_iter()
            .map(|n| node(Kind::Terminal, n, dir.join(n), vec![]))
            .collect();
        let at = t
            .children
            .iter()
            .position(|c| c.kind == Kind::Output && c.path.ends_with("FLOWPLAN"))
            .map_or(0, |i| i + 1);
        t.children
            .insert(at, node(Kind::Folder, tree::TERMS, dir, kids));
    }
}

/// A stray running in a terminal tab is no LiveAgent: it is alive while the pid it saved
/// (exec kept brigd's) runs; the pid file goes once it is gone. Status from its hooks.
pub(super) fn stray_status(dir: &Path) -> Status {
    // Gone: terminated by the user (failed) or just exited (off).
    let gone = || {
        if dir.join(runner::TERMINATED).exists() {
            Status::Failed
        } else {
            Status::Off
        }
    };
    let Ok(pid) = fs::read_to_string(dir.join("pid")) else {
        return gone();
    };
    if !pid.trim().parse().is_ok_and(runner::pid_alive) {
        let _ = fs::remove_file(dir.join("pid"));
        return gone();
    }
    let cwd = fs::read_to_string(dir.join("cwd")).unwrap_or_default();
    agent_glyph_status(
        Some(runner::read_status(dir, Path::new(cwd.trim()))),
        Status::Pending,
    )
}

/// Is any `pid` file under `dir` (a flow agent's, a stray's) a running process?
fn live_pid(dir: &Path) -> bool {
    fs::read_dir(dir).into_iter().flatten().flatten().any(|e| {
        let Ok(t) = e.file_type() else { return false };
        if t.is_dir() {
            e.file_name() != ".diff" && live_pid(&e.path())
        } else {
            e.file_name() == "pid"
                && fs::read_to_string(e.path())
                    .is_ok_and(|p| p.trim().parse().is_ok_and(runner::pid_alive))
        }
    })
}

/// Sets `path`'s mtime to now, creating it empty if missing.
fn touch(path: &Path) {
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|f| f.set_modified(SystemTime::now()));
}

/// The first `term-<n>` not in `taken`.
pub(super) fn fresh_name<'a>(taken: impl Iterator<Item = &'a str>) -> String {
    let taken: HashSet<&str> = taken.collect();
    (1..)
        .map(|n| format!("term-{n}"))
        .find(|n| !taken.contains(n.as_str()))
        .unwrap()
}

impl App {
    /// PAUSE: ESC to every live agent of the thread (flow agents, and the shell tabs strays
    /// run in). RESUME: "continue" + Enter to the same set.
    pub(super) fn toggle_pause(&mut self, thread: &str) {
        let resume = self.paused.contains(thread);
        let strays = tree::stray_dirs(&crate::store::thread_dir(thread))
            .iter()
            .any(|d| !matches!(stray_status(d), Status::Off | Status::Done | Status::Failed));
        let mut targets = live_agents()
            .into_iter()
            .filter(|a| a.thread == thread)
            .collect::<Vec<_>>();
        if strays {
            // ponytail: a shell tab is matched to the thread by $BRIGD_THREAD, not to the exact claude inside it
            targets.extend(
                self.live_shells()
                    .into_iter()
                    .filter(|(_, t)| t == thread)
                    .map(|(a, _)| a),
            );
        }
        let bytes: &[u8] = if resume { b"continue\r" } else { b"\x1b" };
        targets.iter().for_each(|a| {
            let _ = a.write(bytes);
        });
        if resume {
            self.paused.remove(thread);
        } else {
            self.paused.insert(thread.into());
        }
    }

    /// Live shell tabs, each with the thread its $BRIGD_THREAD names.
    pub(super) fn live_shells(&self) -> Vec<(Arc<LiveAgent>, String)> {
        let reg = registry_snapshot();
        reg.iter()
            .filter(|(_, a)| a.alive())
            .filter_map(|(k, a)| Some((a.clone(), self.shell_thread.get(k)?.clone())))
            .collect()
    }

    /// Kills the agent, reverts its changes, keeps its output (crate::engine::terminate), and says how it went.
    fn terminate(&mut self, thread: &str, agent: &str) {
        match crate::engine::terminate(thread, agent) {
            Ok(n) => self.flash(format!(
                "terminated {agent}: reverted {n} file(s), output kept"
            )),
            Err(e) => self.flash(format!("terminate {agent}: {e}")),
        }
        self.refresh();
    }

    pub(super) fn pick(&mut self, act: Act) {
        match act {
            Act::Terminate { thread, agent } => self.terminate(&thread, &agent),
            Act::Delete(t) => self.confirm_delete = Some(t),
            Act::Archive(t) => self.archive(&t),
        }
    }

    /// Stops the thread's agents, strays and terminals and closes its tabs (Delete / Archive come next).
    fn stop_thread(&mut self, thread: &str) {
        crate::engine::stop_thread(thread);
        self.live_shells()
            .into_iter()
            .filter(|(_, t)| t == thread)
            .for_each(|(a, _)| a.kill());
        self.tabs.remove(thread);
        if self.thread == thread {
            self.thread.clear(); // not a thread any more: reflow re-picks the first one
            self.reflow();
        }
        self.focus_main &= self.active_tab().is_some();
        self.paused.remove(thread);
    }

    pub(super) fn archive(&mut self, thread: &str) {
        let to = crate::store::archive_dir(&crate::store::root(), thread);
        if to.exists() {
            return self.flash(format!("archive {thread}: {} already exists", to.display()));
        }
        self.stop_thread(thread);
        match crate::store::archive_thread(&crate::store::root(), thread) {
            Ok(to) => self.flash(format!(
                "archived {thread} to {} (its worktrees stay put)",
                to.display()
            )),
            Err(e) => self.flash(format!("archive {thread}: {e}")),
        }
        self.refresh();
    }

    pub(super) fn delete(&mut self, thread: &str) {
        self.stop_thread(thread);
        match crate::store::delete_thread(&crate::store::root(), thread) {
            Ok(()) => self.flash(format!("deleted {thread}")),
            Err(e) => self.flash(format!("delete {thread}: {e}")),
        }
        self.refresh();
    }

    /// Names of the threads under `root` to auto-archive: idle (own files and claude transcripts
    /// under `home`) for ARCHIVE_AFTER and in no way in use. Threads in use here (an open tab or
    /// a live agent or shell) get `seen` touched instead, so the week counts from their last use.
    pub(super) fn stale_threads(&self, root: &Path, home: &Path, now: u64) -> Vec<String> {
        let in_use: HashSet<String> = (self.tabs.iter())
            .filter(|(_, p)| p.list.iter().any(|x| !x.list.is_empty()))
            .map(|(t, _)| t.clone())
            .chain(live_agents().into_iter().map(|a| a.thread.clone()))
            .chain(self.live_shells().into_iter().map(|(_, t)| t))
            .collect();
        let mut stale = vec![];
        for e in fs::read_dir(root.join("threads"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let (tdir, name) = (e.path(), e.file_name().to_string_lossy().into_owned());
            if !tdir.is_dir() {
                continue;
            }
            if in_use.contains(&name) {
                touch(&tdir.join("seen"));
                continue;
            }
            let idle = now.saturating_sub(
                crate::store::last_active(&tdir)
                    .max(crate::store::transcript_active(root, home, &tdir)),
            );
            if idle >= crate::store::ARCHIVE_AFTER
                && !self.planning.contains(&name)
                && !matches!(
                    self.thread_status.get(&tdir),
                    Some(Status::Running | Status::Queued)
                )
                && !crate::engine::executing(&name)
                && !crate::store::archive_dir(root, &name).exists() // quietly: a name clash
                // ponytail: a recycled pid in a stale pid file keeps its thread
                && !live_pid(&tdir)
            {
                stale.push(name);
            }
        }
        stale
    }

    /// Archives the threads `stale_threads` finds, if this TUI owns tui.pid (as claim_queued does),
    /// and says how many and how to restore one.
    pub(super) fn auto_archive(&mut self) {
        if crate::launch::tui_pid() != Some(std::process::id()) {
            return;
        }
        let (root, home) = (crate::store::root(), crate::store::home());
        let stale = self.stale_threads(&root, &home, crate::store::now());
        stale.iter().for_each(|t| self.archive(t));
        // archive() flashes per thread (and may fail): count the ones that moved.
        let n = stale
            .iter()
            .filter(|t| !crate::store::thread_dir(t).exists())
            .count();
        if n > 0 {
            self.flash(format!(
                "auto-archived {n} thread{}; restore: mv ~/.brigd/archive/threads/<t> ~/.brigd/threads/ && touch ~/.brigd/threads/<t>/seen",
                if n == 1 { "" } else { "s" }
            ));
        }
    }

    /// Runs threads `brigd resume --bg` queued, if this TUI owns tui.pid. Takes over
    /// tui.pid when its owner is gone, so one open TUI always answers.
    pub(super) fn claim_queued(&mut self) {
        let me = std::process::id();
        let owner = crate::launch::tui_pid() == Some(me)
            || (!crate::launch::tui_alive()
                && fs::write(crate::launch::tui_pid_path(), me.to_string()).is_ok());
        let queued: Vec<String> = self
            .thread_status
            .iter()
            .filter_map(|(p, st)| {
                let name = p.file_name()?.to_string_lossy().into_owned();
                crate::engine::claimable(st, owner, crate::engine::executing(&name)).then_some(name)
            })
            .collect();
        for name in queued {
            // Claim on disk before spawning so the next tick never sees it queued again.
            // The status cache may be a tick old: only claim what is still queued on disk.
            let claimed = crate::store::load(&name).and_then(|(_, mut st)| {
                if st.status != Status::Queued {
                    return Ok(false);
                }
                st.status = Status::Running;
                st.updated = crate::store::now(); // reconcile treats a stale `updated` as a dead run
                crate::store::write_json(&crate::store::thread_dir(&name).join("state.json"), &st)
                    .map(|_| true)
            });
            match claimed {
                Ok(false) => {}
                Ok(true) => {
                    self.flash(format!("running queued thread {name}"));
                    self.run_planned(name);
                }
                Err(e) => self.flash(format!("thread {name}: {e}")),
            }
        }
    }

    /// Fails running threads whose claude is gone (see `crate::engine::reconcile`).
    pub(super) fn reconcile_running(&self) {
        for name in self
            .thread_status
            .iter()
            .filter(|(_, st)| **st == Status::Running)
            .filter_map(|(p, _)| Some(p.file_name()?.to_string_lossy().into_owned()))
        {
            if let Ok((_, mut st)) = crate::store::load(&name) {
                let _ = crate::engine::reconcile(&name, &mut st);
            }
        }
    }

    /// y on a planned thread: the same execute as --flow / resume, off the UI thread.
    pub(super) fn run_planned(&mut self, name: String) {
        match crate::store::load(&name) {
            Ok((flow, state)) => {
                let tx = self.tx();
                std::thread::spawn(move || {
                    let r = crate::engine::execute(&name, &flow, state).map_err(|e| e.to_string());
                    let _ = tx.send(Bg::Finished(name, r));
                });
            }
            Err(e) => self.flash(e.to_string()),
        }
    }
}
