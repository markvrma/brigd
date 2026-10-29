//! brigd: plan a flow of Claude agents with the brigd-plan skill, then run it
//! stage by stage, one PTY per agent. Progress lives in ~/.brigd/threads/<name>/
//! ({flowmap.json,state.json,log} and <agent>/{prompt.md,output.md,status,session}),
//! so a thread can be listed, inspected and resumed.

mod engine;
mod flow;
mod launch;
mod runner;
mod store;
mod stray;
mod transcript;
mod tree;
mod tui;

use crate::engine::{reconcile, run_in_tui, thread_busy};
use crate::flow::{flow_from_json, flow_text, plan, print_flow, read_flow, SKILL};
use crate::launch::{background, launch_bg, resume_how, tui_alive, How};
use crate::store::{
    ago, check_new_thread, create_thread, load, now, output_path, root, thread_dir, write_json,
    Flow,
};
use crate::stray::stray;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

/// The app-wide result: errors are shown as text.
pub type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

// Installed by `brigd install-skill`; brigd-plan first, plan-review-go, grill-me, grilling and find-skills back the opt-in plan-review-go path; brigd-resume continues a thread.
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
];

const USAGE: &str = "usage:
  brigd                        open the TUI over all threads
  brigd <name> \"task\"          plan with the brigd-plan skill, confirm, run in the TUI
  brigd <name> --flow <file>   run a hand-written flow
    ... --bg                   skip the confirm, open the TUI in a new herdr tab, tmux window
                               or Terminal window and return; automatic without a TTY or
                               under Claude Code, so Claude can run `brigd <name> --flow <file>`
  brigd ls                     list threads
  brigd status <name>          per-agent detail
  brigd resume <name> [--bg]   continue a stopped thread in the TUI; with --bg (automatic
                               like above) an open brigd TUI runs it, else a new tab/window
  brigd install-skill          install brigd-plan, plan-review-go, grill-me, grilling, brigd-resume, find-skills into ~/.claude/skills";

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
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        [] => tui::run(None),
        ["stray", ..] => stray(&args[1..]),
        ["help" | "-h" | "--help"] => {
            println!("{USAGE}");
            Ok(())
        }
        ["ls"] => ls(),
        ["status", name] => status(name),
        ["resume", name] => {
            let (flow, state) = load(name)?;
            if state.status == "done" {
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
            if bg && state.status == "queued" {
                println!("brigd: thread {name} is already queued");
                return Ok(());
            }
            match resume_how(bg, tui_alive()) {
                How::Here => run_in_tui(name, flow, state),
                How::Queue => {
                    // Re-read so a stale snapshot never overwrites a thread that moved on.
                    let mut state = load(name)?.1;
                    if let Some(pid) = thread_busy(name, &state) {
                        return Err(format!("thread {name} is already running (pid {pid})").into());
                    }
                    state.status = "queued".into();
                    state.updated = now();
                    write_json(&thread_dir(name).join("state.json"), &state)?;
                    println!("brigd: thread {name} queued; the open brigd TUI runs it");
                    Ok(())
                }
                How::Launch => {
                    let at = launch_bg(name, &state.repo)?;
                    println!("brigd: thread {name} running in {at}");
                    Ok(())
                }
            }
        }
        ["install-skill"] => install_skill(),
        [name, "--flow", file] => new_thread(name, read_flow(Path::new(file))?, bg),
        [name, task] if !task.starts_with('-') => {
            check_new_thread(name)?;
            let task = if io::stdin().is_terminal() && !mentions_prg(task) && ask_prg() {
                with_prg(task)
            } else {
                task.to_string()
            };
            eprintln!("planning with opus (brigd-plan)…");
            new_thread(name, plan(&task, &env::current_dir()?)?, bg)
        }
        _ => Err(USAGE.into()),
    }
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
    if bg {
        let plan = dir.join("FLOWPLAN");
        fs::write(&plan, flow_text(&flow))?;
        let at = launch_bg(name, &state.repo)?;
        println!(
            "brigd: thread {name} running in {at}; FLOWPLAN at {}",
            plan.display()
        );
        return Ok(());
    }
    loop {
        write_json(&flow_path, &flow)?;
        print_flow(&flow);
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

/// The commit an agent started from, for its diffs; a relaunch keeps the first one.
fn record_base(cwd: &Path, dir: &Path) {
    if !dir.join("base").exists() {
        if let Some(o) = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
        {
            let _ = fs::write(dir.join("base"), &o.stdout);
        }
    }
}

fn ls() -> Res<()> {
    let root = thread_dir("");
    let mut rows = Vec::new();
    for entry in fs::read_dir(&root).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok((flow, mut st)) = load(&name) {
            let _ = reconcile(&name, &mut st);
            let total: usize = flow.stages.iter().map(Vec::len).sum();
            let done = st.agents.values().filter(|a| a.status == "done").count();
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

fn status(name: &str) -> Res<()> {
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
            let status = if s.status.is_empty() {
                "pending"
            } else {
                &s.status
            };
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

fn install_skill() -> Res<()> {
    for (name, text) in SKILLS {
        let dir = PathBuf::from(env::var("HOME")?)
            .join(".claude/skills")
            .join(name);
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("SKILL.md"), text)?;
        println!("installed {}", dir.join("SKILL.md").display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {}
