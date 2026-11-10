//! Running a flow: stage by stage, each agent in its PTY, supervised until it writes its
//! output.md; plus reconciling dead runs and stopping or terminating agents.

use crate::diffs::{record_base, ROUND};
use crate::flow::{read_flow, task_prompt};
use crate::store::{
    home, load, log, now, output_path, root, set_agent, thread_dir, update, write_json, Agent,
    Flow, State, Status,
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
        let mut outcome = run_stage(name, &flow, i, &state);
        // Loops run one after another, once the whole stage is done.
        for a in flow.stages[i].iter().filter(|a| a.r#loop.is_some()) {
            if matches!(outcome, Ok(true)) {
                outcome = run_loop(name, &flow, a, &state);
            }
        }
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
/// A loop reviewer with a verdict is done too: a crash mid-loop leaves the rest to run_loop.
fn run_stage(thread: &str, flow: &Flow, i: usize, state: &Mutex<State>) -> Res<bool> {
    let dir = thread_dir(thread);
    let finished = |a: &Agent| {
        let saved = state
            .lock()
            .unwrap()
            .agents
            .get(&a.name)
            .is_some_and(|x| x.status == Status::Done);
        saved || first_mark(a).is_some() && marked(&output_path(thread, &a.name), first_mark(a))
    };
    // Before starting any, so a missing bin never leaves half a stage running unsupervised.
    if let Some(a) = flow.stages[i]
        .iter()
        .find(|a| !finished(a) && !agents::on_path(&a.agent))
    {
        return Err(format!("{}: `{}` is not on PATH", a.name, a.agent).into());
    }
    let mut jobs = Vec::new();
    for a in &flow.stages[i] {
        if finished(a) {
            if first_mark(a).is_some() {
                set_agent(state, &a.name, Status::Done)?; // its verdict is in, whatever a crash left
            }
            continue;
        }
        if let Some(live) = runner::live(thread, &a.name).filter(|l| l.alive()) {
            jobs.push((a, live)); // still live from an earlier execute: just supervise it
            continue;
        }
        let cwd = agent_cwd(thread, a, state)?;
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
            .map(|(a, live)| s.spawn(move || supervise(thread, a, live, state, first_mark(a))))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or(false))
            .collect()
    });
    Ok(results.into_iter().all(|ok| ok))
}

/// Where agent `a` works: the repo, or its worktree (created if missing).
fn agent_cwd(thread: &str, a: &Agent, state: &Mutex<State>) -> Res<PathBuf> {
    let repo = PathBuf::from(state.lock().unwrap().repo.clone());
    match a.worktree.as_deref().filter(|k| !k.is_empty()) {
        Some(key) => ensure_worktree(&repo, thread, key),
        None => Ok(repo),
    }
}

/// A loop reviewer's verdict line, and what closes its `to` agent's round.
const VERDICT: &str = "VERDICT:";
const FILES: &str = "## Files changed";

/// What supervise waits for in output.md beyond its existing: None = nothing; Some((k, mark)) = a
/// `mark` line after the `## Round k` heading (k = 0: anywhere). A closing line, so a round
/// half written before a crash never counts.
type Mark = Option<(u32, &'static str)>;

/// A loop reviewer's first run must end with a verdict.
fn first_mark(a: &Agent) -> Mark {
    a.r#loop.as_ref().map(|_| (0, VERDICT))
}

/// `## Round <k>...` → k.
fn round_no(l: &str) -> Option<u32> {
    let d = l.strip_prefix("## Round ")?;
    d[..d.find(|c: char| !c.is_ascii_digit()).unwrap_or(d.len())]
        .parse()
        .ok()
}

/// The highest `## Round k` in an output.md (0: none): the TUI's loop counter.
pub(crate) fn rounds(out: &Path) -> u32 {
    let text = fs::read_to_string(out).unwrap_or_default();
    text.lines().filter_map(round_no).max().unwrap_or(0)
}

/// The rest of a line that starts with `mark`, past list/bold/code decoration (`- **VERDICT: pass**`).
fn after_mark<'a>(l: &'a str, mark: &str) -> Option<&'a str> {
    l.trim_start_matches(|c| "-*` ".contains(c))
        .strip_prefix(mark)
}

/// output.md exists and has `mark` (see Mark).
fn marked(out: &Path, mark: Mark) -> bool {
    let Ok(text) = fs::read_to_string(out) else {
        return false;
    };
    let Some((k, m)) = mark else {
        return true;
    };
    let mut lines = text.lines();
    (k == 0 || lines.any(|l| round_no(l) == Some(k))) && lines.any(|l| after_mark(l, m).is_some())
}

/// A loop reviewer's verdict: its last `VERDICT:` line says pass.
fn passed(out: &Path) -> bool {
    let text = fs::read_to_string(out).unwrap_or_default();
    text.lines()
        .rev()
        .find_map(|l| after_mark(l, VERDICT))
        .is_some_and(|v| {
            v.trim_start_matches(|c| "*` ".contains(c))
                .to_lowercase()
                .starts_with("pass")
        })
}

/// Reviewer `rev`'s loop (store::Loop): while its last verdict is fail, up to `max` rounds of
/// its `to` agent fixing, then `rev` re-reviewing, typed into their sessions. Stateless: a
/// round whose closing marks are in output.md is skipped, so a resumed thread goes on from there.
fn run_loop(thread: &str, flow: &Flow, rev: &Agent, state: &Mutex<State>) -> Res<bool> {
    let Some(l) = &rev.r#loop else {
        return Ok(true);
    };
    let to = flow
        .agent(&l.to)
        .ok_or_else(|| format!("{}: no loop agent {}", rev.name, l.to))?;
    let (rout, tout) = (
        output_path(thread, &rev.name),
        output_path(thread, &to.name),
    );
    let task = |a: &Agent| thread_dir(thread).join(&a.name).join("prompt.md");
    for k in 1..=l.max {
        if passed(&rout) {
            break;
        }
        log(
            thread,
            &format!("{}: fail, round {k}/{} → {}", rev.name, l.max, to.name),
        );
        let fix = format!(
            "Round {k}/{}: {} found problems: see the latest `## Round` section (or the whole file in round 1) of {}. \
             Fix them. Then APPEND to your output {} a `## Round {k}` section (a short summary of what you changed) \
             followed by its own `## Files changed` list. Your task: {}",
            l.max,
            rev.name,
            rout.display(),
            tout.display(),
            task(to).display()
        );
        if !rerun(thread, to, k, FILES, &fix, state)? {
            return Ok(false);
        }
        let review = format!(
            "Round {k}/{}: {} addressed your findings: see the `## Round {k}` section of {}. Re-review the current \
             changes. Then APPEND to your output {} a `## Round {k}` section with your findings and a \
             `VERDICT: pass` or `VERDICT: fail` line. Your task: {}",
            l.max,
            to.name,
            tout.display(),
            rout.display(),
            task(rev).display()
        );
        if !rerun(thread, rev, k, VERDICT, &review, state)? {
            return Ok(false);
        }
    }
    if passed(&rout) {
        log(thread, &format!("{}: pass", rev.name));
    } else {
        log(
            thread,
            &format!("{}: loop cap hit ({} rounds), moving on", rev.name, l.max),
        );
    }
    Ok(true)
}

/// Round `k` of a loop for agent `a`: types `msg` into its live session once it is idle, else
/// resumes its claude session with it, or starts a pi/kimi agent fresh on it. Then supervises
/// it until output.md closes `## Round k` with `mark`. Already closed (a resumed thread): true.
/// Never records a new base: its diffs stay cumulative from its first run.
fn rerun(
    thread: &str,
    a: &Agent,
    k: u32,
    mark: &'static str,
    msg: &str,
    state: &Mutex<State>,
) -> Res<bool> {
    let dir = thread_dir(thread).join(&a.name);
    let mark = Some((k, mark));
    if marked(&dir.join("output.md"), mark) {
        return Ok(true);
    }
    // A terminated member stays dead: terminate marked it Failed. (stop_thread leaves the
    // status alone, so a stopped thread still resumes.)
    let terminated = || dir.join(runner::TERMINATED).exists();
    if terminated()
        && load(thread)?
            .1
            .agents
            .get(&a.name)
            .is_some_and(|s| s.status == Status::Failed)
    {
        return Ok(false);
    }
    // ROUND first, so diffs cannot freeze again in between.
    fs::write(dir.join(ROUND), k.to_string())?;
    let _ = fs::remove_dir_all(dir.join(".diff")); // its frozen diffs: this round adds to them
    set_agent(state, &a.name, Status::Running)?;
    let alive = || runner::live(thread, &a.name).filter(|l| l.alive());
    let start = || -> Res<Arc<runner::LiveAgent>> {
        if let Some(l) = alive() {
            // ponytail: waits for as long as it is blocked on the user or busy with a message typed by hand.
            while l.alive() && l.status() != Status::Idle && !terminated() {
                thread::sleep(TICK);
            }
        }
        if terminated() {
            return Err(format!("{} terminated", a.name).into());
        }
        // "working" before the message: a stale idle must not count as done or nudge, and the
        // hooks' own working/idle can only come after it.
        let working = || fs::write(dir.join("status"), "working");
        Ok(match alive() {
            Some(l) => {
                if !l.pty_status {
                    working()?;
                }
                type_msg(&l, msg)?;
                l
            }
            None if a.agent == "claude" => {
                let l = runner::resume_agent(thread, &a.name, Some(msg))?;
                working()?;
                l
            }
            None => {
                let cwd = agent_cwd(thread, a, state)?;
                runner::start_agent(thread, a, &cwd, &dir, msg)?
            }
        })
    };
    let live = match start() {
        Ok(l) => l,
        Err(e) => {
            // No supervise to clean up after it: do not leave round/Running behind.
            let _ = fs::remove_file(dir.join(ROUND));
            let _ = set_agent(state, &a.name, Status::Failed);
            return Err(e);
        }
    };
    log(thread, &format!("{} round {k}", a.name));
    let ok = supervise(thread, a, live, state, mark);
    let _ = fs::remove_file(dir.join(ROUND));
    Ok(ok)
}

/// Types `msg` into an agent's prompt: Ctrl-U first clears anything typed there, the Enter
/// comes after PASTE_GAP so it is not read as part of a paste.
fn type_msg(live: &runner::LiveAgent, msg: &str) -> Res<()> {
    live.write(b"\x15")?;
    live.write(msg.as_bytes())?;
    thread::sleep(PASTE_GAP);
    live.write(b"\r")
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

/// Drives one agent to completion: done = output.md exists (with `mark`) once it is idle (or
/// exited). One nudge when it goes idle without output. Blocked prompts are left for the user. A
/// non-claude agent (no hooks) that goes quiet again after the nudge counts as blocked.
fn supervise(
    thread: &str,
    a: &Agent,
    live: Arc<runner::LiveAgent>,
    state: &Mutex<State>,
    mark: Mark,
) -> bool {
    let out = live.dir.join("output.md");
    let how = if live.pty_status {
        ""
    } else {
        " with AskUserQuestion"
    };
    let o = out.display();
    let want = match mark {
        None => format!("writing your result file. If the task is fully done, write your final result to {o}."),
        Some((0, m)) => format!("a `{m}` line in your result file {o}. If the task is fully done, write it there with that line."),
        Some((k, m)) => format!("finishing round {k}. If it is done, APPEND to {o} a `## Round {k}` section ending with a `{m}` line."),
    };
    let nudge = format!(
        "You stopped without {want} \
         If it is not done and you cannot finish it, do not write it: ask the user{how} \
         (a few options plus free text), unless the user already told you to stop. Task: {}",
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
        if marked(&out, mark) && (!alive || status == Status::Idle) {
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
        if let Err(e) = type_msg(&live, &nudge) {
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
    fn round_marks() {
        let d = env::temp_dir().join(format!("brigd-marks-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        let out = d.join("output.md");
        assert!(!marked(&out, None) && !passed(&out) && rounds(&out) == 0);
        fs::write(&out, "x\n## Files changed\n## Round 1: fixes\nhalf written").unwrap();
        assert!(marked(&out, None) && !marked(&out, Some((1, FILES))));
        assert!(!marked(&out, Some((0, VERDICT))) && rounds(&out) == 1);
        fs::write(
            &out,
            "VERDICT: pass\n## Round 1\n`VERDICT: fail`\n## Round 10\n",
        )
        .unwrap();
        assert!(marked(&out, Some((1, VERDICT))) && !marked(&out, Some((2, VERDICT))));
        assert!(!passed(&out) && rounds(&out) == 10); // the last verdict counts
        fs::write(&out, "VERDICT: fail\n## Round 1\n**VERDICT: Pass**\n").unwrap();
        assert!(passed(&out));
        let _ = fs::remove_dir_all(&d);
    }

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
case "$d" in
*/impl) printf 'r\n## Files changed\n' > "$d/output.md"; printf idle > "$d/status"; k=0
  while read line; do k=$((k+1)); echo "$line" >> "$d/typed"
    printf '## Round %s\nfixed\n## Files changed\n' $k >> "$d/output.md"; printf idle > "$d/status"; done; exit;;
*/rev*) printf 'VERDICT: fail\n' > "$d/output.md"; printf idle > "$d/status"; k=0; v=pass
  case "$d" in *cap) v=fail;; esac
  while read line; do k=$((k+1)); echo "$line" >> "$d/typed"
    printf '## Round %s\n- **VERDICT: %s**\n' $k $v >> "$d/output.md"; printf idle > "$d/status"; done; exit;;
esac
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

        // Loops: impl and its reviewer stay alive in their PTYs; each round is typed into them.
        let looped = |th: &str, rev: &str, max: u32| {
            let mut r = agent(rev, Some("wt"), "default");
            r.r#loop = Some(crate::store::Loop {
                to: "impl".into(),
                max,
            });
            let flow = Flow {
                goal: "g".into(),
                stages: vec![vec![agent("impl", Some("wt"), "acceptEdits")], vec![r]],
            };
            fs::create_dir_all(thread_dir(th)).unwrap();
            write_json(&thread_dir(th).join("flowmap.json"), &flow).unwrap();
            let state = State {
                thread: th.into(),
                repo: repo_s.clone(),
                status: Status::Planned,
                ..Default::default()
            };
            execute(th, &flow, state).unwrap();
            let (_, st) = load(th).unwrap();
            assert_eq!(st.status, Status::Done);
            assert!(st.agents.values().all(|a| a.status == Status::Done));
            let d = thread_dir(th);
            let read = |f: &str| fs::read_to_string(d.join(f)).unwrap_or_default();
            for a in ["impl", rev] {
                let out = read(&format!("{a}/output.md"));
                assert!(
                    out.contains("## Round 1\n") && !out.contains("## Round 2"),
                    "{out}"
                );
                assert!(!d.join(a).join(ROUND).exists());
            }
            assert!(read("impl/typed").starts_with(&format!("Round 1/{max}: {rev} found problems")));
            assert!(read(&format!("{rev}/typed"))
                .starts_with(&format!("Round 1/{max}: impl addressed")));
            assert!(read(&format!("{rev}/prompt.md")).contains("`VERDICT: pass`"));
            read("FLOWPLAN")
        };
        let log = looped("lp", "rev", 3);
        assert!(
            log.contains("rev: fail, round 1/3 → impl") && log.contains("rev: pass"),
            "{log}"
        );
        let log = looped("lc", "revcap", 1);
        assert!(log.contains("revcap: loop cap hit (1 rounds)"), "{log}");
        for a in runner::registry().lock().unwrap().values() {
            a.kill();
        }
        let _ = fs::remove_dir_all(&t);
    }
}
