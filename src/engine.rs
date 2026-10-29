//! Running a flow: stage by stage, each agent in its PTY, supervised until it writes its
//! output.md; plus reconciling dead runs and stopping or terminating agents.

use crate::diffs::record_base;
use crate::flow::{read_flow, task_prompt};
use crate::store::{
    home, load, log, now, root, set_agent, thread_dir, update, write_json, Agent, Flow, State,
    Status,
};
use crate::{agents, runner, transcript, tree, Res};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fs, thread};

/// Runs the flow from `state.stage` on and returns when it is done or a stage
/// failed. Blocks, so the TUI calls it from a background thread. Never prints:
/// progress goes to the thread log, state.json and the runner registry.
pub(crate) fn execute(name: &str, flow: &Flow, state: State) -> Res<()> {
    if !executing_set().insert(name.into()) {
        return Err(format!("thread {name} is already running").into());
    }
    struct Running<'a>(&'a str);
    impl Drop for Running<'_> {
        fn drop(&mut self) {
            executing_set().remove(self.0);
        }
    }
    let _running = Running(name);
    let mut i = state.stage;
    let mut flow = flow.clone();
    let state = Mutex::new(state);
    update(&state, |s| s.status = Status::Running)?;
    loop {
        // flowmap.json is live: agents may rewrite later stages, so reread it before every stage.
        let path = thread_dir(name).join("flowmap.json");
        match read_flow(&path) {
            Ok(f) => flow = f,
            Err(e) => log(
                name,
                &format!("{} invalid, keeping previous flow: {e}", path.display()),
            ),
        }
        if i >= flow.stages.len() {
            break;
        }
        update(&state, |s| s.stage = i)?;
        log(
            name,
            &format!("── {name}: stage {}/{} ──", i + 1, flow.stages.len()),
        );
        let outcome = run_stage(name, &flow, i, &state);
        if !matches!(outcome, Ok(true)) {
            update(&state, |s| s.status = Status::Failed)?;
            let why = outcome.err().map(|e| format!(": {e}")).unwrap_or_default();
            let msg = format!("stage {} failed{why}", i + 1);
            log(name, &msg);
            return Err(msg.into());
        }
        i += 1;
    }
    update(&state, |s| {
        s.status = Status::Done;
        s.stage = flow.stages.len();
    })?;
    log(name, "done");
    Ok(())
}

/// Threads `execute` is running in this process.
static EXECUTING: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

fn executing_set() -> std::sync::MutexGuard<'static, BTreeSet<String>> {
    EXECUTING.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn executing(name: &str) -> bool {
    executing_set().contains(name)
}

/// Starts every unfinished agent of stage `i`, waits for all, returns true if all are done.
fn run_stage(thread: &str, flow: &Flow, i: usize, state: &Mutex<State>) -> Res<bool> {
    let dir = thread_dir(thread);
    let repo = PathBuf::from(state.lock().unwrap().repo.clone());
    // Before starting any, so a missing bin never leaves half a stage running unsupervised.
    let finished = |n: &str| {
        state
            .lock()
            .unwrap()
            .agents
            .get(n)
            .is_some_and(|x| x.status == Status::Done)
    };
    if let Some(a) = flow.stages[i]
        .iter()
        .find(|a| !finished(&a.name) && !agents::on_path(&a.agent))
    {
        return Err(format!("{}: `{}` is not on PATH", a.name, a.agent).into());
    }
    let mut jobs = Vec::new();
    for a in &flow.stages[i] {
        let done = state
            .lock()
            .unwrap()
            .agents
            .get(&a.name)
            .is_some_and(|s| s.status == Status::Done);
        if done {
            continue;
        }
        if let Some(live) = runner::live(thread, &a.name).filter(|l| l.alive()) {
            jobs.push((a, live)); // still live from an earlier execute: just supervise it
            continue;
        }
        let key = a.worktree.as_deref().unwrap_or("");
        let cwd = if key.is_empty() {
            repo.clone()
        } else {
            ensure_worktree(&repo, thread, key)?
        };
        let adir = dir.join(&a.name);
        fs::create_dir_all(&adir)?;
        fs::write(adir.join("prompt.md"), task_prompt(thread, flow, i, a))?;
        let prompt = format!(
            "Read {} and do the task in it.",
            adir.join("prompt.md").display()
        );
        log(thread, &format!("start {} in {}", a.name, cwd.display()));
        record_base(&cwd, &adir);
        // A trust dialog for a new worktree shows in the agent's own terminal; the user answers it there.
        let live = runner::start_agent(thread, a, &cwd, &adir, &prompt)?;
        set_agent(state, &a.name, Status::Running)?;
        jobs.push((a, live));
    }

    let results: Vec<bool> = thread::scope(|s| {
        let handles: Vec<_> = jobs
            .into_iter()
            .map(|(a, live)| s.spawn(move || supervise(thread, a, live, state)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or(false))
            .collect()
    });
    Ok(results.into_iter().all(|ok| ok))
}

/// ~/.brigd/worktrees/<thread>/<key> on branch brigd/<thread>/<key>, created or reused.
fn ensure_worktree(repo: &Path, thread: &str, key: &str) -> Res<PathBuf> {
    let path = tree::space_cwd(&root(), repo, thread, key);
    if path.exists() {
        return Ok(path);
    }
    fs::create_dir_all(path.parent().unwrap_or(&path))?;
    let git = |args: &[&str]| Command::new("git").arg("-C").arg(repo).args(args).output();
    let (branch, p) = (
        format!("brigd/{thread}/{key}"),
        path.to_string_lossy().into_owned(),
    );
    let _ = git(&["worktree", "prune"]); // forget checkouts deleted by hand
    let mut out = git(&["worktree", "add", "-b", &branch, &p])?;
    if !out.status.success() {
        out = git(&["worktree", "add", &p, &branch])?; // the branch outlived an earlier checkout
    }
    if !out.status.success() {
        return Err(format!(
            "git worktree add {p}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(path)
}

/// supervise's clock: one tick per second.
const TICK: Duration = Duration::from_secs(1);
/// Ticks between subagent syncs.
const SYNC_EVERY: u32 = 5;
/// Ticks after a nudge before idle counts again (a nudge needs a moment to show as working).
const NUDGE_COOLDOWN: u32 = 30;
/// Idle ticks in a row before an agent counts as stopped.
const IDLE_DEBOUNCE: u32 = 3;
/// Between typing the nudge and its Enter, so the Enter is not read as part of a paste.
const PASTE_GAP: Duration = Duration::from_millis(300);

/// Drives one agent to completion: done = output.md exists once it is idle (or exited).
/// One nudge when it goes idle without output. Blocked prompts are left for the user. A
/// non-claude agent (no hooks) that goes quiet again after the nudge counts as blocked.
fn supervise(thread: &str, a: &Agent, live: Arc<runner::LiveAgent>, state: &Mutex<State>) -> bool {
    let out = live.dir.join("output.md");
    let how = if live.pty_status {
        ""
    } else {
        " with AskUserQuestion"
    };
    let nudge = format!(
        "You stopped without writing your result file. If the task is fully done, write your final result to {}. \
         If it is not done and you cannot finish it, do not write it: ask the user{how} \
         (a few options plus free text), unless the user already told you to stop. Task: {}",
        out.display(),
        live.dir.join("prompt.md").display()
    );
    let home = home();
    let sync = || transcript::sync_agent(&home, &live.cwd, &live.dir);
    let (mut tick, mut idle_for, mut nudged_at, mut blocked) = (0u32, 0, None::<u32>, false);
    let ok = loop {
        thread::sleep(TICK);
        tick += 1;
        if tick % SYNC_EVERY == 0 {
            sync();
        }
        let (alive, status) = (live.alive(), live.status());
        if live.dir.join(runner::TERMINATED).exists() {
            log(thread, &format!("{} terminated", a.name));
            break false;
        }
        if out.exists() && (!alive || status == Status::Idle) {
            log(thread, &format!("{} done", a.name));
            break true;
        }
        if !alive {
            log(
                thread,
                &format!("{} exited without {}", a.name, out.display()),
            );
            break false;
        }
        // A quiet non-claude agent after its nudge: blocked until it prints again.
        let quiet_after_nudge = live.pty_status
            && status == Status::Idle
            && nudged_at.is_some_and(|t| tick - t >= NUDGE_COOLDOWN);
        if status == Status::Blocked || quiet_after_nudge {
            if !blocked {
                blocked = true;
                let _ = set_agent(state, &a.name, Status::Blocked);
                log(thread, &format!("{} needs you (blocked)", a.name));
            }
            continue;
        }
        if blocked {
            blocked = false;
            let _ = set_agent(state, &a.name, Status::Running);
        }
        // "starting" (e.g. a trust dialog) and "working" just wait. A nudge needs a moment to show as working.
        if status != Status::Idle || nudged_at.is_some_and(|t| tick - t < NUDGE_COOLDOWN) {
            idle_for = 0;
            continue;
        }
        idle_for += 1;
        // ponytail: fixed 3s debounce on idle; Stop hooks are exact, the transcript fallback is not.
        if idle_for < IDLE_DEBOUNCE {
            continue;
        }
        if nudged_at.is_some() {
            log(
                thread,
                &format!("{} stopped without {}", a.name, out.display()),
            );
            break false;
        }
        nudged_at = Some(tick);
        idle_for = 0;
        log(thread, &format!("{} idle without output, nudging", a.name));
        let typed = live.write(nudge.as_bytes()).and_then(|_| {
            thread::sleep(PASTE_GAP);
            live.write(b"\r")
        });
        if let Err(e) = typed {
            log(thread, &format!("{}: nudge failed: {e}", a.name));
            break false;
        }
    };
    sync();
    let _ = set_agent(
        state,
        &a.name,
        if ok { Status::Done } else { Status::Failed },
    );
    ok
}

/// A live claude (this or another brigd) on a thread marked running. A running status alone
/// can be stale after a crash, so the agents' pid files decide.
pub(crate) fn thread_busy(name: &str, state: &State) -> Option<u32> {
    if state.status != Status::Running {
        return None;
    }
    state
        .agents
        .keys()
        .find_map(|a| runner::running_elsewhere(&thread_dir(name).join(a)))
}

/// Marks a thread failed when it says running but no claude is alive: this process is not
/// executing it, nothing touched it for 30s and no agent pid file has a live claude.
/// Leaves `updated` alone (ls sorts by it). True when it changed the state.
pub(crate) fn reconcile(name: &str, state: &mut State) -> Res<bool> {
    if state.status != Status::Running
        || executing(name)
        || now().saturating_sub(state.updated) <= 30
        || thread_busy(name, state).is_some()
    {
        return Ok(false);
    }
    for a in state.agents.values_mut() {
        if matches!(a.status, Status::Running | Status::Blocked) {
            a.status = Status::Failed;
        }
    }
    state.status = Status::Failed;
    write_json(&thread_dir(name).join("state.json"), state)?;
    log(name, "stale: no live agent, marked failed");
    Ok(true)
}

/// The TUI claims a thread only if it is queued, this TUI owns tui.pid, and this
/// process is not already executing it.
pub(crate) fn claimable(status: &Status, owner: bool, busy: bool) -> bool {
    *status == Status::Queued && owner && !busy
}

/// Terminates `agent` (a flow agent, or "stray agents/<n>"): kills its claude, reverts the
/// files it changed to its base commit (diffs::revert), keeps its dir and output.md, and
/// marks it failed (state.json for a flow agent, the dir's `terminated` file for both).
/// Returns how many files were reverted.
pub(crate) fn terminate(thread: &str, agent: &str) -> Res<usize> {
    let dir = thread_dir(thread).join(agent);
    let (cwd, repo, state) = if Path::new(agent).starts_with(tree::STRAY) {
        let cwd = PathBuf::from(
            fs::read_to_string(dir.join("cwd"))
                .map_err(|e| format!("{thread}/{agent}: no cwd: {e}"))?
                .trim(),
        );
        (cwd.clone(), cwd, None)
    } else {
        let (flow, state) = load(thread)?;
        let a = flow
            .agent(agent)
            .ok_or_else(|| format!("no agent {agent} in thread {thread}"))?;
        let repo = PathBuf::from(&state.repo);
        (
            tree::space_cwd(&root(), &repo, thread, a.worktree.as_deref().unwrap_or("")),
            repo,
            Some(state),
        )
    };
    stop_agent(thread, agent)?;
    if runner::agent_of(&dir) != "claude" {
        log(
            thread,
            &format!("{agent}: revert covers reported files only"),
        );
    }
    let n = crate::diffs::revert(&home(), &cwd, &repo, &dir)?;
    if let Some(state) = state {
        set_agent(&Mutex::new(state), agent, Status::Failed)?;
    }
    Ok(n)
}

/// stop_agent polls every KILL_POLL for KILL_POLLS polls, sending SIGKILL at poll KILL_ESCALATE.
const KILL_POLL: Duration = Duration::from_millis(100);
const KILL_POLLS: u32 = 40;
const KILL_ESCALATE: u32 = 20;

/// Kills `agent`'s claude (its LiveAgent, or a stray's pid) and waits for it to be gone.
/// Marks it `terminated` first, so a supervise thread that sees it die does not report it done.
/// Does NOT revert its files (terminate does).
fn stop_agent(thread: &str, agent: &str) -> Res<()> {
    let dir = thread_dir(thread).join(agent);
    fs::write(dir.join(runner::TERMINATED), "")?;
    let pid = fs::read_to_string(dir.join("pid")).ok(); // a stray running in a terminal tab
    if let Some(l) = runner::live(thread, agent) {
        l.kill();
    }
    if let Some(pid) = &pid {
        let _ = Command::new("kill").arg(pid.trim()).status();
    }
    // Wait for the process to be gone so it cannot rewrite a file after the revert.
    for i in 0..KILL_POLLS {
        if !runner::live(thread, agent).is_some_and(|l| l.alive())
            && !pid
                .as_ref()
                .is_some_and(|p| p.trim().parse().is_ok_and(runner::pid_alive))
        {
            break;
        }
        if i == KILL_ESCALATE {
            if let Some(pid) = &pid {
                let _ = Command::new("kill").args(["-9", pid.trim()]).status(); // ignored SIGTERM
            }
        }
        thread::sleep(KILL_POLL);
    }
    let _ = fs::remove_file(dir.join("pid"));
    Ok(())
}

/// Stops every running agent of `thread`: its live flow agents and its strays with a live pid.
/// Does NOT revert anything, unlike terminate (an archived thread keeps its work).
pub(crate) fn stop_thread(thread: &str) {
    let mut agents: Vec<String> = runner::registry()
        .lock()
        .unwrap()
        .values()
        .filter(|a| a.thread == thread && a.alive())
        .map(|a| a.agent.clone())
        .collect();
    agents.extend(
        tree::stray_dirs(&thread_dir(thread))
            .into_iter()
            .filter(|d| {
                fs::read_to_string(d.join("pid"))
                    .is_ok_and(|p| p.trim().parse().is_ok_and(runner::pid_alive))
            })
            .filter_map(|d| Some(tree::agent_id(&d, &d.file_name()?.to_string_lossy()))),
    );
    for a in agents {
        let _ = stop_agent(thread, &a);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::tests::agent;
    use std::env;

    #[test]
    fn claimable_rules() {
        assert!(claimable(&Status::Queued, true, false));
        assert!(!claimable(&Status::Queued, false, false)); // another TUI owns tui.pid
        assert!(!claimable(&Status::Queued, true, true)); // already executing here
        assert!(!claimable(&Status::Stopped, true, false));
    }

    /// Runs a two-stage flow against a fake `claude` on PATH: the second agent works in a
    /// fresh worktree and goes idle once without output, so it needs the nudge typed into its PTY.
    #[test]
    fn execute_with_fake_claude() {
        let t = env::temp_dir().join(format!("brigd-exec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&t);
        let (bin, repo) = (t.join("bin"), t.join("repo"));
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let fake = r#"#!/bin/sh
for x; do last=$x; done
p=${last#Read }; p=${p% and do the task in it.}; d=$(dirname "$p")
printf '%s\n' "$@" > "$d/args"; pwd > "$d/cwd"
case "$d" in *nudge) printf idle > "$d/status"; read line; echo "$line" > "$d/nudged";; esac
echo result > "$d/output.md"; printf idle > "$d/status"; sleep 1
"#;
        fs::write(bin.join("claude"), fake).unwrap();
        Command::new("chmod")
            .arg("+x")
            .arg(bin.join("claude"))
            .status()
            .unwrap();
        let git = |a: &[&str]| {
            assert!(Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(a)
                .output()
                .unwrap()
                .status
                .success())
        };
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "init"]);
        env::set_var("HOME", &t);
        env::set_var(
            "PATH",
            format!("{}:{}", bin.display(), env::var("PATH").unwrap()),
        );

        let flow = Flow {
            goal: "g".into(),
            stages: vec![
                vec![agent("first", None, "default")],
                vec![agent("nudge", Some("wt"), "acceptEdits")],
            ],
        };
        let repo_s = repo.canonicalize().unwrap().to_string_lossy().into_owned();
        fs::create_dir_all(thread_dir("th")).unwrap();
        write_json(&thread_dir("th").join("flowmap.json"), &flow).unwrap();
        let state = State {
            thread: "th".into(),
            repo: repo_s.clone(),
            status: Status::Planned,
            ..Default::default()
        };
        execute("th", &flow, state).unwrap();

        let (_, st) = load("th").unwrap();
        assert_eq!(st.status, Status::Done);
        assert!(st.agents.values().all(|a| a.status == Status::Done));
        let d = thread_dir("th");
        assert_eq!(
            fs::read_to_string(d.join("first/cwd")).unwrap().trim(),
            repo_s
        );
        let wt = t.join(".brigd/worktrees/th/wt");
        assert_eq!(
            fs::read_to_string(d.join("nudge/cwd")).unwrap().trim(),
            wt.canonicalize().unwrap().to_string_lossy()
        );
        assert!(fs::read_to_string(d.join("nudge/nudged"))
            .unwrap()
            .starts_with("You stopped without writing"));
        assert!(fs::read_to_string(d.join("nudge/prompt.md"))
            .unwrap()
            .contains(&d.join("first/output.md").display().to_string()));
        assert!(fs::read_to_string(d.join("first/prompt.md"))
            .unwrap()
            .contains(&d.join("flowmap.json").display().to_string()));
        let args = fs::read_to_string(d.join("first/args")).unwrap();
        assert!(
            args.contains("--session-id\n")
                && args.contains("--settings\n")
                && args.contains("--add-dir=")
        );
        assert_eq!(
            fs::read_to_string(d.join("first/session")).unwrap().len(),
            36
        );
        assert!(fs::read_to_string(d.join("FLOWPLAN"))
            .unwrap()
            .contains("nudging"));

        // reconcile: a fresh `updated` is left alone; a stale one with dead pids fails the thread.
        let (_, mut st) = load("th").unwrap();
        st.status = Status::Running;
        st.agents.get_mut("nudge").unwrap().status = Status::Running;
        st.updated = now();
        let fresh = serde_json::to_string(&st).unwrap();
        assert!(!reconcile("th", &mut st).unwrap());
        assert_eq!(serde_json::to_string(&st).unwrap(), fresh);
        let mut dead = Command::new("true").spawn().unwrap();
        dead.wait().unwrap();
        for a in ["first", "nudge"] {
            fs::write(d.join(a).join("pid"), dead.id().to_string()).unwrap();
        }
        st.updated = 0;
        assert!(reconcile("th", &mut st).unwrap());
        let (_, st) = load("th").unwrap();
        assert_eq!((st.status, st.updated), (Status::Failed, 0));
        assert_eq!(st.agents["nudge"].status, Status::Failed);
        assert_eq!(st.agents["first"].status, Status::Done);
        let _ = fs::remove_dir_all(&t);
    }
}
