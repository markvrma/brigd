//! brigd: plan a flow of Claude agents with the brigd-plan skill, then run it
//! stage by stage, one PTY per agent. Progress lives in ~/.brigd/threads/<name>/
//! ({flowmap.json,state.json,FLOWPLAN} and <agent>/{prompt.md,output.md,status,session}),
//! so a thread can be listed, inspected and resumed. This file is the CLI.
//!
//! Modules: store (thread data on disk), flow (the flow definition and planner), engine
//! (running a flow), launch (`--bg` and terminal surfaces), stray (the `claude` shim),
//! runner (agent PTYs), tree (the sidebar model), diffs (agent diffs), transcript
//! (claude's transcript files), tui (the screen).
//!
//! "Flow" names five things: `store::Flow`, the stages of agents, saved as a thread's
//! flowmap.json (agents may edit later stages while it runs); FLOWPLAN, that flow rendered
//! as a tree plus the run log; an agent's flow.md, a mermaid chart of the steps it took;
//! `--flow <file>`, a hand-written flowmap.json to run; and the `flow` module.

mod agents;
mod diffs;
mod engine;
mod flow;
mod launch;
mod runner;
mod store;
mod stray;
mod transcript;
mod tree;
mod tui;

use crate::engine::{execute, reconcile, thread_busy};
use crate::flow::{flow_from_json, plan, read_flow, SKILL};
use crate::launch::{background, launch_bg, resume_how, tui_alive, ResumeMode};
use crate::store::{
    ago, check_new_thread, create_thread, home, load, now, output_path, root, thread_dir,
    write_json, Flow, State, Status, RESERVED,
};
use crate::stray::stray;
use crate::tui::flow_text;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs, thread};

/// The app-wide result: errors are shown as text.
pub type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

// Installed by `brigd install-skill`; brigd-plan first, plan-review-go, grill-me, grilling and find-skills back the opt-in plan-review-go path; brigd-resume continues a thread; brigd-master runs the master session.
const SKILLS: &[(&str, &str)] = &[
    ("brigd-plan", SKILL),
    (
        "plan-review-go",
        include_str!("../skills/plan-review-go/SKILL.md"),
    ),
    ("grill-me", include_str!("../skills/grill-me/SKILL.md")),
    ("grilling", include_str!("../skills/grilling/SKILL.md")),
    (
        "brigd-resume",
        include_str!("../skills/brigd-resume/SKILL.md"),
    ),
    (
        "find-skills",
        include_str!("../skills/find-skills/SKILL.md"),
    ),
    (
        "brigd-master",
        include_str!("../skills/brigd-master/SKILL.md"),
    ),
];

const USAGE: &str = "usage:
  brigd                        open the TUI over all threads
  brigd <name> \"task\"          plan with the brigd-plan skill, confirm, run in the TUI
  brigd <name> --flow <file>   run a hand-written flow
    ... --bg                   skip the confirm and return: an open brigd TUI runs it, else a
                               new herdr tab, tmux window or Terminal window; automatic without
                               a TTY or under Claude Code, so Claude can run `brigd <name> --flow <file>`
  brigd ls                     list threads
  brigd status <name>          per-agent detail
  brigd resume <name> [--bg]   continue a stopped thread in the TUI; with --bg (automatic
                               like above) an open brigd TUI runs it, else a new tab/window
  brigd master                 the master session: tell it what you want, it starts threads
                               (also the TUI's pinned master row; one at a time)
  brigd install-skill          install brigd-plan, plan-review-go, grill-me, grilling, brigd-resume,
                               find-skills, brigd-master into each configured agent's skills dir";

fn main() {
    if let Err(e) = real_main() {
        eprintln!("brigd: {e}");
        std::process::exit(1);
    }
}

fn real_main() -> Res<()> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let bg_flag = args.iter().any(|a| a == "--bg");
    args.retain(|a| a != "--bg");
    let tty = io::stdin().is_terminal() && io::stdout().is_terminal();
    let bg = background(bg_flag, tty, |k| env::var(k).ok());
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    // Only what plans or runs agents needs the agent config (and its first-run questions).
    if matches!(argv.as_slice(), [] | ["resume", _] | [_, "--flow", _])
        || matches!(argv.as_slice(), [name, task] if !RESERVED.contains(name) && !task.starts_with('-'))
    {
        agents::init(!bg)?;
    }
    match argv.as_slice() {
        [] => tui::run(None),
        ["stray", ..] => stray(&args[1..]),
        ["help" | "-h" | "--help"] => {
            println!("{USAGE}");
            Ok(())
        }
        ["ls"] => cmd_ls(),
        ["status", name] => cmd_status(name),
        ["resume", name] => cmd_resume(name, bg),
        ["install-skill"] => install_skill(),
        ["master"] => cmd_master(),
        [name, "--flow", file] => new_thread(name, read_flow(Path::new(file))?, bg),
        [name, task] if !task.starts_with('-') => cmd_new(name, task, bg),
        _ => Err(USAGE.into()),
    }
}

/// `brigd resume <name>`: run a stopped thread here, queue it for the open TUI, or launch it
/// in a new terminal (see launch::resume_how).
fn cmd_resume(name: &str, bg: bool) -> Res<()> {
    let (flow, state) = load(name)?;
    if state.status == Status::Done {
        return Err(format!("thread {name} is already done").into());
    }
    // A thread saved with N at the confirm has no FLOWPLAN yet.
    let plan = thread_dir(name).join("FLOWPLAN");
    if !plan.exists() {
        fs::write(plan, flow_text(&flow))?;
    }
    if let Some(pid) = thread_busy(name, &state) {
        return Err(format!("thread {name} is already running (pid {pid})").into());
    }
    if bg && state.status == Status::Queued {
        println!("brigd: thread {name} is already queued");
        return Ok(());
    }
    match resume_how(bg, tui_alive()) {
        ResumeMode::Here => run_in_tui(name, flow, state),
        ResumeMode::Queue => queue(name),
        ResumeMode::Launch => {
            let at = launch_bg(name, &state.repo)?;
            println!("brigd: thread {name} running in {at}");
            Ok(())
        }
    }
}

/// Marks thread `name` queued: the open TUI (tui.pid) claims and runs it.
fn queue(name: &str) -> Res<()> {
    // Re-read so a stale snapshot never overwrites a thread that moved on.
    let mut state = load(name)?.1;
    if let Some(pid) = thread_busy(name, &state) {
        return Err(format!("thread {name} is already running (pid {pid})").into());
    }
    state.status = Status::Queued;
    state.updated = now();
    write_json(&thread_dir(name).join("state.json"), &state)?;
    println!("brigd: thread {name} queued; the open brigd TUI runs it");
    Ok(())
}

/// `brigd <name> "task"`: plan the task (asking about plan-review-go on a TTY), then new_thread.
fn cmd_new(name: &str, task: &str, bg: bool) -> Res<()> {
    check_new_thread(name)?;
    let task = if io::stdin().is_terminal() && !mentions_prg(task) && ask_prg() {
        with_prg(task)
    } else {
        task.to_string()
    };
    eprintln!("planning with {} (brigd-plan)…", agents::config().base);
    new_thread(name, plan(&task, &env::current_dir()?)?, bg)
}

/// Whether the task already opts into plan-review-go (brigd-plan looks for this name).
fn mentions_prg(task: &str) -> bool {
    task.contains("plan-review-go")
}

fn with_prg(task: &str) -> String {
    format!("{task}\n\nUse plan-review-go.")
}

/// One line from stdin: does the user want plan-review-go?
fn ask_prg() -> bool {
    eprint!("Use plan-review-go? (y/N) ");
    let mut line = String::new();
    io::stdin().read_line(&mut line).is_ok()
        && matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

fn new_thread(name: &str, mut flow: Flow, bg: bool) -> Res<()> {
    check_new_thread(name)?;
    let repo = env::current_dir()?
        .canonicalize()?
        .to_string_lossy()
        .into_owned();
    let state = create_thread(&root(), name, &repo, &flow)?;
    let dir = thread_dir(name);
    let flow_path = dir.join("flowmap.json");
    // Like resume: with --bg the open TUI runs it, else a new terminal; without, the confirm below.
    let how = resume_how(bg, tui_alive());
    if how != ResumeMode::Here {
        let plan = dir.join("FLOWPLAN");
        fs::write(&plan, flow_text(&flow))?;
        if how == ResumeMode::Queue {
            return queue(name);
        }
        let at = launch_bg(name, &state.repo)?;
        println!(
            "brigd: thread {name} running in {at}; FLOWPLAN at {}",
            plan.display()
        );
        return Ok(());
    }
    loop {
        write_json(&flow_path, &flow)?;
        println!("\n{}", flow_text(&flow));
        print!("run thread {name}? [y/N/e(dit)] ");
        io::stdout().flush()?;
        let mut ans = String::new();
        io::stdin().read_line(&mut ans)?;
        match ans.trim() {
            "y" | "Y" => {
                fs::write(dir.join("FLOWPLAN"), flow_text(&flow))?;
                return run_in_tui(name, flow, state);
            }
            "e" | "E" => {
                let editor = env::var("EDITOR").unwrap_or_else(|_| "vi".into());
                Command::new(editor).arg(&flow_path).status()?;
                match flow_from_json(&fs::read_to_string(&flow_path)?) {
                    Ok(f) => flow = f,
                    Err(e) => eprintln!("edited flow invalid, keeping previous one: {e}"),
                }
            }
            _ => {
                println!("saved, not run. `brigd resume {name}` runs it.");
                return Ok(());
            }
        }
    }
}

/// Runs the thread's flow on a background thread while the TUI owns the screen.
fn run_in_tui(name: &str, flow: Flow, state: State) -> Res<()> {
    let n = name.to_string();
    let job = thread::spawn(move || execute(&n, &flow, state));
    tui::run(Some(name))?;
    match job.is_finished().then(|| job.join()) {
        Some(Ok(Ok(()))) => println!("thread {name} done"),
        Some(Ok(Err(e))) => println!("thread {name}: {e}"),
        Some(Err(_)) => println!(
            "thread {name}: runner panicked, see {}",
            thread_dir(name).join("FLOWPLAN").display()
        ),
        None => {
            println!("left brigd, agents stopped. `brigd resume {name}` continues thread {name}.")
        }
    }
    Ok(())
}

fn cmd_ls() -> Res<()> {
    let root = thread_dir("");
    let mut rows = Vec::new();
    for entry in fs::read_dir(&root).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok((flow, mut st)) = load(&name) {
            let _ = reconcile(&name, &mut st);
            let total: usize = flow.stages.iter().map(Vec::len).sum();
            let done = st
                .agents
                .values()
                .filter(|a| a.status == Status::Done)
                .count();
            let stage = format!(
                "{}/{}",
                (st.stage + 1).min(flow.stages.len()),
                flow.stages.len()
            );
            rows.push((
                st.updated,
                format!(
                    "{name:<13} {:<8} {stage:<6} {done}/{total:<5} {:<9} {}",
                    st.status,
                    ago(st.updated),
                    st.repo
                ),
            ));
        }
    }
    if rows.is_empty() {
        println!("no threads. start one: brigd <name> \"task\"");
        return Ok(());
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    println!(
        "{:<13} {:<8} {:<6} {:<7} {:<9} REPO",
        "NAME", "STATUS", "STAGE", "AGENTS", "UPDATED"
    );
    rows.iter().for_each(|r| println!("{}", r.1));
    Ok(())
}

fn cmd_status(name: &str) -> Res<()> {
    let (flow, mut st) = load(name)?;
    let _ = reconcile(name, &mut st);
    println!(
        "{name}: {} (stage {}/{}), updated {}\nrepo: {}\ngoal: {}",
        st.status,
        (st.stage + 1).min(flow.stages.len()),
        flow.stages.len(),
        ago(st.updated),
        st.repo,
        flow.goal
    );
    for (i, stage) in flow.stages.iter().enumerate() {
        println!("stage {}", i + 1);
        for a in stage {
            let s = st.agents.get(&a.name).cloned().unwrap_or_default();
            let status = s.status;
            let out = output_path(name, &a.name);
            let out = if out.exists() {
                out.display().to_string()
            } else {
                "-".into()
            };
            println!(
                "  {:<19} {:<8} {:<7} wt={:<12} {}",
                a.name,
                status,
                a.model,
                a.worktree.as_deref().unwrap_or("-"),
                out
            );
        }
    }
    Ok(())
}

/// `brigd master`: the master session in this terminal. The TUI's pinned master row runs
/// the same session, so only one of them can hold it at a time.
fn cmd_master() -> Res<()> {
    let dir = runner::master_dir();
    if let Some(pid) = runner::running_elsewhere(&dir) {
        return Err(format!(
            "the master session is already running (pid {pid}), e.g. in the brigd TUI"
        )
        .into());
    }
    let mut child = Command::new("claude")
        .args(runner::master_args(&dir)?)
        .current_dir(&dir)
        .spawn()?;
    // So the TUI's master row does not start a second claude on this session.
    fs::write(dir.join("pid"), child.id().to_string())?;
    child.wait()?;
    Ok(())
}

/// Writes SKILLS into each configured agent's skills dir (agents on PATH before the first run, else claude).
fn install_skill() -> Res<()> {
    let cfg: Option<agents::Config> = fs::read_to_string(root().join("config.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let mut dirs: Vec<PathBuf> = cfg
        .map_or_else(
            || {
                let f: Vec<String> = agents::AGENTS
                    .iter()
                    .filter(|a| agents::on_path(a))
                    .map(|a| a.to_string())
                    .collect();
                if f.is_empty() {
                    vec!["claude".into()]
                } else {
                    f
                }
            },
            |c| c.agents,
        )
        .iter()
        .map(|a| agents::skills_dir(a, &home()))
        .collect();
    dirs.sort();
    dirs.dedup();
    for (dir, (name, text)) in dirs.iter().flat_map(|d| SKILLS.iter().map(move |s| (d, s))) {
        let dir = dir.join(name);
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("SKILL.md"), text)?;
        println!("installed {}", dir.join("SKILL.md").display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {}
