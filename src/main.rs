//! brigd: plan a flow of Claude agents with the brigd-plan skill, then run it
//! stage by stage, one PTY per agent. Progress lives in ~/.brigd/threads/<name>/
//! ({flowmap.json,state.json,log} and <agent>/{prompt.md,output.md,status,session}),
//! so a thread can be listed, inspected and resumed.

mod runner;
mod tree;
mod tui;

use runner::Res;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, thread};

const SKILL: &str = include_str!("../skills/brigd-plan/SKILL.md");
const RESERVED: &[&str] = &["ls", "status", "resume", "install-skill", "help", "stray"];
const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const MODES: &[&str] = &["default", "acceptEdits", "auto"];
const WRITE_MODES: &[&str] = &["acceptEdits", "auto"];
// Keeps "<thread>/<agent>" labels short enough for a sidebar.
const THREAD_MAX: usize = 12;
const AGENT_MAX: usize = 19;

const USAGE: &str = "usage:
  brigd                        open the TUI over all threads
  brigd <name> \"task\"          plan with the brigd-plan skill, confirm, run in the TUI
  brigd <name> --flow <file>   run a hand-written flow
    ... --bg                   skip the confirm, open the TUI in a new herdr tab, tmux window
                               or Terminal window and return; automatic without a TTY or
                               under Claude Code, so Claude can run `brigd <name> --flow <file>`
  brigd ls                     list threads
  brigd status <name>          per-agent detail
  brigd resume <name>          continue a stopped thread in the TUI
  brigd install-skill          install brigd-plan into ~/.claude/skills";

const SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["goal","stages"],
"properties":{"goal":{"type":"string"},
"stages":{"type":"array","minItems":1,"items":{"type":"array","minItems":1,"items":{
"type":"object","additionalProperties":false,
"required":["name","model","effort","worktree","task","files","mode"],
"properties":{
"name":{"type":"string","pattern":"^[a-z][a-z0-9_-]{0,18}$"},
"model":{"type":"string"},
"effort":{"type":"string","enum":["low","medium","high","xhigh","max"]},
"worktree":{"type":["string","null"]},
"task":{"type":"string"},
"files":{"type":"array","items":{"type":"string"}},
"mode":{"type":"string","enum":["default","acceptEdits","auto"]}}}}}}}"#;

#[derive(Serialize, Deserialize, Clone)]
struct Agent {
    name: String,
    model: String,
    effort: String,
    #[serde(default)]
    worktree: Option<String>,
    task: String,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default = "default_mode")]
    mode: String,
}

fn default_mode() -> String {
    "acceptEdits".into()
}

#[derive(Serialize, Deserialize, Clone)]
struct Flow {
    goal: String,
    stages: Vec<Vec<Agent>>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct AgentState {
    status: String,
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    thread: String,
    repo: String,
    status: String,
    stage: usize,
    updated: u64,
    agents: BTreeMap<String, AgentState>,
}

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
        ["help" | "-h" | "--help"] => Ok(println!("{USAGE}")),
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
            run_in_tui(name, flow, state)
        }
        ["install-skill"] => install_skill(),
        [name, "--flow", file] => {
            let flow: Flow = serde_json::from_str(&fs::read_to_string(file)?)?;
            validate(&flow)?;
            new_thread(name, flow, bg)
        }
        [name, task] if !task.starts_with('-') => {
            check_new_thread(name)?;
            new_thread(name, plan(task)?, bg)
        }
        _ => Err(USAGE.into()),
    }
}

// ---------- threads on disk ----------

/// ~/.brigd
fn root() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_default()).join(".brigd")
}

fn thread_dir(name: &str) -> PathBuf {
    root().join("threads").join(name)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn load(name: &str) -> Res<(Flow, State)> {
    let dir = thread_dir(name);
    let read = |f: &str| fs::read_to_string(dir.join(f)).map_err(|e| format!("thread {name}: {f}: {e}"));
    Ok((serde_json::from_str(&read("flowmap.json").or_else(|_| read("flow.json"))?)?, serde_json::from_str(&read("state.json")?)?))
}

/// Writes via temp file + rename so a crash never leaves half a journal.
fn write_json(path: &Path, v: &impl Serialize) -> Res<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_string_pretty(v)?)?;
    Ok(fs::rename(tmp, path)?)
}

fn update(state: &Mutex<State>, f: impl FnOnce(&mut State)) -> Res<()> {
    let mut s = state.lock().unwrap();
    f(&mut s);
    s.updated = now();
    write_json(&thread_dir(&s.thread).join("state.json"), &*s)
}

fn set_agent(state: &Mutex<State>, agent: &str, status: &str) -> Res<()> {
    update(state, |s| s.agents.entry(agent.into()).or_default().status = status.into())
}

/// Terminates `agent` (a flow agent, or "stray agents/<n>"): kills its claude, reverts the
/// files it changed to its base commit (tree::revert), keeps its dir and output.md, and
/// marks it failed (state.json for a flow agent, the dir's `terminated` file for both).
/// Returns how many files were reverted.
fn terminate(thread: &str, agent: &str) -> Res<usize> {
    let dir = thread_dir(thread).join(agent);
    let (cwd, repo, state) = if Path::new(agent).starts_with(tree::STRAY) {
        let cwd = PathBuf::from(fs::read_to_string(dir.join("cwd")).map_err(|e| format!("{thread}/{agent}: no cwd: {e}"))?.trim());
        (cwd.clone(), cwd, None)
    } else {
        let (flow, state) = load(thread)?;
        let a = flow.stages.iter().flatten().find(|a| a.name == agent).ok_or_else(|| format!("no agent {agent} in thread {thread}"))?;
        let repo = PathBuf::from(&state.repo);
        (tree::space_cwd(&root(), &repo, thread, a.worktree.as_deref().unwrap_or("")), repo, Some(state))
    };
    // First, so a supervise thread that sees the process die does not report it done.
    fs::write(dir.join(runner::TERMINATED), "")?;
    let pid = fs::read_to_string(dir.join("pid")).ok(); // a stray running in a terminal tab
    if let Some(l) = runner::live(thread, agent) {
        l.kill();
    }
    if let Some(pid) = &pid {
        let _ = Command::new("kill").arg(pid.trim()).status();
    }
    // Wait for the process to be gone so it cannot rewrite a file after the revert.
    for i in 0..40 {
        if !runner::live(thread, agent).is_some_and(|l| l.alive()) && !pid.as_ref().is_some_and(|p| runner::pid_alive(p)) {
            break;
        }
        if i == 20 {
            if let Some(pid) = &pid {
                let _ = Command::new("kill").args(["-9", pid.trim()]).status(); // ignored SIGTERM
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = fs::remove_file(dir.join("pid"));
    let n = tree::revert(&PathBuf::from(env::var("HOME").unwrap_or_default()), &cwd, &repo, &dir)?;
    if let Some(state) = state {
        set_agent(&Mutex::new(state), agent, "failed")?;
    }
    Ok(n)
}

/// Appends to ~/.brigd/threads/<thread>/FLOWPLAN, the plan followed by the live run log.
/// execute never prints: the TUI owns the screen and shows this file.
fn log(thread: &str, msg: &str) {
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(thread_dir(thread).join("FLOWPLAN")) {
        let pad = if msg.starts_with('─') { "" } else { "  " };
        let _ = writeln!(f, "{pad}{msg}");
    }
}

fn check_new_thread(name: &str) -> Res<()> {
    if RESERVED.contains(&name) || !valid_name(name, THREAD_MAX) {
        return Err(format!("bad thread name {name:?}: [a-z][a-z0-9_-], max {THREAD_MAX}, not one of {RESERVED:?}").into());
    }
    if thread_dir(name).exists() {
        return Err(format!("thread {name} exists: `brigd resume {name}`, or delete {} to reuse the name", thread_dir(name).display()).into());
    }
    Ok(())
}

fn new_thread(name: &str, mut flow: Flow, bg: bool) -> Res<()> {
    check_new_thread(name)?;
    let dir = thread_dir(name);
    fs::create_dir_all(&dir)?;
    let flow_path = dir.join("flowmap.json");
    let repo = env::current_dir()?.canonicalize()?.to_string_lossy().into_owned();
    let state = State { thread: name.into(), repo, status: "planned".into(), updated: now(), ..Default::default() };
    write_json(&dir.join("state.json"), &state)?;
    if bg {
        write_json(&flow_path, &flow)?;
        let plan = dir.join("FLOWPLAN");
        fs::write(&plan, flow_text(&flow))?;
        let at = launch_bg(name, &state.repo)?;
        println!("brigd: thread {name} running in {at}; FLOWPLAN at {}", plan.display());
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
                match serde_json::from_str::<Flow>(&fs::read_to_string(&flow_path)?).map_err(|e| e.to_string()).and_then(|f| validate(&f).map(|_| f)) {
                    Ok(f) => flow = f,
                    Err(e) => eprintln!("edited flow invalid, keeping previous one: {e}"),
                }
            }
            _ => return Ok(println!("saved, not run. `brigd resume {name}` runs it.")),
        }
    }
}

// ---------- background launch ----------

/// True when nobody can answer the y/N confirm or watch a fullscreen TUI here: `--bg`,
/// no TTY on stdin/stdout, or Claude Code's Bash tool (it sets CLAUDECODE=1).
fn background(bg_flag: bool, tty: bool, var: impl Fn(&str) -> Option<String>) -> bool {
    bg_flag || !tty || var("CLAUDECODE").is_some_and(|v| !v.is_empty())
}

/// Vars Claude Code's Bash tool sets that describe the calling session (seen in its env).
const PARENT_SESSION_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_PID",
    "AI_AGENT",
];

#[derive(Debug, PartialEq)]
enum Surface {
    Herdr(String),
    Tmux,
    Terminal,
}

/// Terminal surfaces that can host `brigd resume`, best first.
fn surfaces(var: impl Fn(&str) -> Option<String>) -> Vec<Surface> {
    let set = |k: &str| var(k).filter(|v| !v.is_empty());
    let mut v = vec![];
    if let (Some(_), Some(ws)) = (set("HERDR_ENV"), set("HERDR_WORKSPACE_ID")) {
        v.push(Surface::Herdr(ws));
    }
    if set("TMUX").is_some() {
        v.push(Surface::Tmux);
    }
    if cfg!(target_os = "macos") {
        v.push(Surface::Terminal);
    }
    v
}

/// Opens `brigd resume <name>` in a new terminal surface and says where. The herdr
/// server, tmux server or Terminal.app owns that process, so it is detached from this
/// one and from the caller's terminal; brigd only runs short helper commands.
fn launch_bg(name: &str, repo: &str) -> Res<String> {
    let q = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    // A tmux window inherits this process's env: drop the calling Claude session's vars so agents start clean.
    let unset: String = PARENT_SESSION_VARS.iter().map(|v| format!("-u {v} ")).collect();
    let cmd = format!("env {unset}{} resume {name}", q(&env::current_exe()?.to_string_lossy()));
    let label = format!("brigd-{name}");
    let mut errs = vec![];
    for s in surfaces(|k| env::var(k).ok()) {
        let r = match &s {
            Surface::Herdr(ws) => herdr_tab(ws, repo, &label, &cmd),
            // `|| read` keeps the window open on an error so it can be read.
            Surface::Tmux => run_ok(Command::new("tmux").args(["new-window", "-d", "-n", &label, "-c", repo, &format!("{cmd} || read _")]))
                .map(|_| format!("tmux window {label} (tmux select-window -t {label})")),
            Surface::Terminal => {
                let sh = format!("cd {} && {cmd}", q(repo));
                let script = format!("tell application \"Terminal\" to do script \"{}\"", sh.replace('\\', "\\\\").replace('"', "\\\""));
                run_ok(Command::new("osascript").args(["-e", &script])).map(|_| "a new Terminal window".to_string())
            }
        };
        match r {
            Ok(at) => return Ok(at),
            Err(e) => errs.push(format!("{s:?}: {e}")),
        }
    }
    let why = if errs.is_empty() { "not in herdr or tmux, not on macOS".into() } else { errs.join("; ") };
    Err(format!("no terminal to run thread {name} in ({why}); it is saved, `brigd resume {name}` in a terminal runs it").into())
}

/// Opens an unfocused herdr tab in workspace `ws` and runs `cmd` in its shell.
fn herdr_tab(ws: &str, repo: &str, label: &str, cmd: &str) -> Res<String> {
    let out = run_ok(Command::new("herdr").args(["tab", "create", "--workspace", ws, "--cwd", repo, "--label", label, "--no-focus"]))?;
    let v: serde_json::Value = serde_json::from_str(&out)?;
    let pane = v["result"]["root_pane"]["pane_id"].as_str().ok_or_else(|| format!("no root pane in {out}"))?;
    // A fresh pane reports busy for a few seconds while its shell starts.
    let mut last = String::new();
    for _ in 0..20 {
        match run_ok(Command::new("herdr").args(["pane", "run", pane, cmd])) {
            Ok(_) => return Ok(format!("herdr tab {label} (workspace {ws}, pane {pane})")),
            Err(e) => last = e.to_string(),
        }
        thread::sleep(Duration::from_millis(500));
    }
    Err(format!("herdr pane run {pane}: {last}").into())
}

/// Runs a short helper command; returns its stdout, or its stderr as the error.
fn run_ok(c: &mut Command) -> Res<String> {
    let out = c.stdin(Stdio::null()).output()?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string().into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---------- planning ----------

fn plan(task: &str) -> Res<Flow> {
    let mut prompt = format!("Plan a brigd flow for this task:\n\n{task}");
    for attempt in 0..2 {
        eprintln!("planning with opus (brigd-plan)…");
        // ponytail: skill text passed as system prompt so it works uninstalled; install-skill is for interactive /brigd-plan.
        let mut cmd = Command::new("claude");
        // brigd may run under Claude Code; the planner is a session of its own.
        PARENT_SESSION_VARS.iter().for_each(|v| _ = cmd.env_remove(v));
        let out = cmd
            .args(["-p", &prompt, "--model", "opus", "--effort", "high", "--output-format", "json"])
            .args(["--no-session-persistence", "--json-schema", SCHEMA, "--tools", "Read,Grep,Glob"])
            .args(["--append-system-prompt", SKILL])
            .output()?;
        let v: serde_json::Value = serde_json::from_slice(&out.stdout)
            .map_err(|e| format!("planner output not JSON ({e}): {}", String::from_utf8_lossy(&out.stderr)))?;
        let so = v.get("structured_output").cloned().ok_or_else(|| format!("planner returned no structured_output: {v}"))?;
        let flow: Flow = serde_json::from_value(so.clone())?;
        match validate(&flow) {
            Ok(()) => return Ok(flow),
            Err(e) if attempt == 0 => prompt += &format!("\n\nYour previous flow was invalid: {e}\nPrevious flow: {so}\nReturn a fixed flow."),
            Err(e) => return Err(format!("planner flow still invalid: {e}\n{}", serde_json::to_string_pretty(&so)?).into()),
        }
    }
    unreachable!()
}

fn valid_name(s: &str, max: usize) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= max
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

fn validate(flow: &Flow) -> Result<(), String> {
    if flow.stages.is_empty() {
        return Err("flow has no stages".into());
    }
    let mut names = HashSet::new();
    for (i, stage) in flow.stages.iter().enumerate() {
        let n = i + 1;
        if stage.is_empty() {
            return Err(format!("stage {n} is empty"));
        }
        let mut editors: HashMap<&str, usize> = HashMap::new();
        for a in stage {
            let who = format!("stage {n} agent {:?}", a.name);
            if !valid_name(&a.name, AGENT_MAX) {
                return Err(format!("{who}: name must be [a-z][a-z0-9_-], max {AGENT_MAX}"));
            }
            if a.name == "log" {
                return Err(format!("{who}: \"log\" is reserved (the thread log file)"));
            }
            if !names.insert(a.name.as_str()) {
                return Err(format!("{who}: duplicate name"));
            }
            if a.model.trim().is_empty() || a.task.trim().is_empty() {
                return Err(format!("{who}: model and task are required"));
            }
            if !EFFORTS.contains(&a.effort.as_str()) {
                return Err(format!("{who}: effort must be one of {EFFORTS:?}"));
            }
            if !MODES.contains(&a.mode.as_str()) {
                return Err(format!("{who}: mode must be one of {MODES:?}"));
            }
            let wt = a.worktree.as_deref().unwrap_or("");
            if !wt.is_empty() && !valid_name(wt, AGENT_MAX) {
                return Err(format!("{who}: worktree must be null or [a-z][a-z0-9_-], max {AGENT_MAX}"));
            }
            let c = editors.entry(wt).or_default();
            *c += WRITE_MODES.contains(&a.mode.as_str()) as usize;
            let space = if wt.is_empty() { "main repo".into() } else { format!("worktree {wt:?}") };
            if *c > 1 {
                return Err(format!("stage {n}: two editing agents ({WRITE_MODES:?}) in {space}; move one to the next stage"));
            }
        }
    }
    Ok(())
}

fn print_flow(flow: &Flow) {
    println!("\n{}", flow_text(flow));
}

fn flow_text(flow: &Flow) -> String {
    let mut t = format!("goal: {}\n", flow.goal);
    for (i, stage) in flow.stages.iter().enumerate() {
        t += &format!("stage {} ({} parallel)\n", i + 1, stage.len());
        for a in stage {
            let wt = a.worktree.as_deref().unwrap_or("-");
            t += &format!("  {:<19} {:<7} {:<6} wt={:<12} {:<11} {}\n", a.name, a.model, a.effort, wt, a.mode, first_line(&a.task));
        }
    }
    t
}

fn first_line(s: &str) -> String {
    let l = s.lines().next().unwrap_or("");
    if l.chars().count() > 60 { l.chars().take(57).collect::<String>() + "..." } else { l.into() }
}

// ---------- running ----------

/// Runs the thread's flow on a background thread while the TUI owns the screen.
fn run_in_tui(name: &str, flow: Flow, state: State) -> Res<()> {
    let n = name.to_string();
    let job = thread::spawn(move || execute(&n, &flow, state));
    tui::run(Some(name))?;
    match job.is_finished().then(|| job.join()) {
        Some(Ok(Ok(()))) => println!("thread {name} done"),
        Some(Ok(Err(e))) => println!("thread {name}: {e}"),
        Some(Err(_)) => println!("thread {name}: runner panicked, see {}", thread_dir(name).join("FLOWPLAN").display()),
        None => println!("left brigd, agents stopped. `brigd resume {name}` continues thread {name}."),
    }
    Ok(())
}

/// Runs the flow from `state.stage` on and returns when it is done or a stage
/// failed. Blocks, so the TUI calls it from a background thread. Never prints:
/// progress goes to the thread log, state.json and the runner registry.
fn execute(name: &str, flow: &Flow, state: State) -> Res<()> {
    let mut i = state.stage;
    let mut flow = flow.clone();
    let state = Mutex::new(state);
    update(&state, |s| s.status = "running".into())?;
    loop {
        // flowmap.json is live: agents may rewrite later stages, so reread it before every stage.
        let path = thread_dir(name).join("flowmap.json");
        match fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str::<Flow>(&t).map_err(|e| e.to_string())).and_then(|f| validate(&f).map(|_| f)) {
            Ok(f) => flow = f,
            Err(e) => log(name, &format!("{} invalid, keeping previous flow: {e}", path.display())),
        }
        if i >= flow.stages.len() {
            break;
        }
        update(&state, |s| s.stage = i)?;
        log(name, &format!("── {name}: stage {}/{} ──", i + 1, flow.stages.len()));
        let outcome = run_stage(name, &flow, i, &state);
        if !matches!(outcome, Ok(true)) {
            update(&state, |s| s.status = "failed".into())?;
            let why = outcome.err().map(|e| format!(": {e}")).unwrap_or_default();
            let msg = format!("stage {} failed{why}", i + 1);
            log(name, &msg);
            return Err(msg.into());
        }
        i += 1;
    }
    update(&state, |s| {
        s.status = "done".into();
        s.stage = flow.stages.len();
    })?;
    log(name, "done");
    Ok(())
}

/// Starts every unfinished agent of stage `i`, waits for all, returns true if all are done.
fn run_stage(thread: &str, flow: &Flow, i: usize, state: &Mutex<State>) -> Res<bool> {
    let dir = thread_dir(thread);
    let repo = PathBuf::from(state.lock().unwrap().repo.clone());
    let mut jobs = Vec::new();
    for a in &flow.stages[i] {
        let done = state.lock().unwrap().agents.get(&a.name).is_some_and(|s| s.status == "done");
        if done {
            continue;
        }
        if let Some(live) = runner::live(thread, &a.name).filter(|l| l.alive()) {
            jobs.push((a, live)); // still live from an earlier execute: just supervise it
            continue;
        }
        let key = a.worktree.as_deref().unwrap_or("");
        let cwd = if key.is_empty() { repo.clone() } else { ensure_worktree(&repo, thread, key)? };
        let adir = dir.join(&a.name);
        fs::create_dir_all(&adir)?;
        fs::write(adir.join("prompt.md"), task_prompt(thread, flow, i, a))?;
        let prompt = format!("Read {} and do the task in it.", adir.join("prompt.md").display());
        log(thread, &format!("start {} in {}", a.name, cwd.display()));
        record_base(&cwd, &adir);
        // A trust dialog for a new worktree shows in the agent's own terminal; Mark answers it there.
        let live = runner::start_claude(thread, &a.name, &cwd, &adir, &claude_flags(thread, a), &prompt)?;
        set_agent(state, &a.name, "running")?;
        jobs.push((a, live));
    }

    let results: Vec<bool> = thread::scope(|s| {
        let handles: Vec<_> = jobs.into_iter().map(|(a, live)| s.spawn(move || supervise(thread, a, live, state))).collect();
        handles.into_iter().map(|h| h.join().unwrap_or(false)).collect()
    });
    Ok(results.into_iter().all(|ok| ok))
}

/// The commit an agent started from, for its diffs; a relaunch keeps the first one.
fn record_base(cwd: &Path, dir: &Path) {
    if !dir.join("base").exists() {
        if let Some(o) = Command::new("git").arg("-C").arg(cwd).args(["rev-parse", "HEAD"]).output().ok().filter(|o| o.status.success()) {
            let _ = fs::write(dir.join("base"), &o.stdout);
        }
    }
}

/// claude flags for an agent, minus the session/hooks/prompt the runner adds.
/// Variadic options use --flag=value so they cannot swallow the trailing prompt.
fn claude_flags(thread: &str, a: &Agent) -> Vec<String> {
    let dir = thread_dir(thread);
    let sys = format!(
        "BRIGD WORKER {thread}/{}: do the task. You may use the Task/Agent tool for subagents, \
         but never run the `claude` CLI or start other claude sessions. \
         When finished, write your final result to {}.",
        a.name,
        dir.join(&a.name).join("output.md").display()
    );
    let s = |x: &str| x.to_string();
    vec![
        s("--model"), a.model.clone(), s("--effort"), a.effort.clone(), s("--permission-mode"), a.mode.clone(),
        s("-n"), a.name.clone(),
        // The thread dir sits outside the agent's cwd; grant it so reading the task and writing the result never prompt.
        format!("--add-dir={}", dir.display()),
        // Edit rules cover every file-writing tool; claude rejects Write(path) rules.
        s("--allowedTools=Read(~/.brigd/threads/**),Edit(~/.brigd/threads/**)"),
        s("--append-system-prompt"), sys,
    ]
}

/// ~/.brigd/worktrees/<thread>/<key> on branch brigd/<thread>/<key>, created or reused.
fn ensure_worktree(repo: &Path, thread: &str, key: &str) -> Res<PathBuf> {
    let path = tree::space_cwd(&root(), repo, thread, key);
    if path.exists() {
        return Ok(path);
    }
    fs::create_dir_all(path.parent().unwrap_or(&path))?;
    let git = |args: &[&str]| Command::new("git").arg("-C").arg(repo).args(args).output();
    let (branch, p) = (format!("brigd/{thread}/{key}"), path.to_string_lossy().into_owned());
    let _ = git(&["worktree", "prune"]); // forget checkouts deleted by hand
    let mut out = git(&["worktree", "add", "-b", &branch, &p])?;
    if !out.status.success() {
        out = git(&["worktree", "add", &p, &branch])?; // the branch outlived an earlier checkout
    }
    if !out.status.success() {
        return Err(format!("git worktree add {p}: {}", String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    Ok(path)
}

fn output_path(thread: &str, agent: &str) -> PathBuf {
    thread_dir(thread).join(agent).join("output.md")
}

fn task_prompt(thread: &str, flow: &Flow, i: usize, a: &Agent) -> String {
    let mut p = format!(
        "# brigd task: {} (thread {thread}, stage {}/{})\n\nGoal of the whole thread: {}\n\n## Task\n{}\n",
        a.name,
        i + 1,
        flow.stages.len(),
        flow.goal,
        a.task
    );
    if !a.files.is_empty() {
        p += "\n## Files to check\n";
        for f in &a.files {
            p += &format!("- {f}\n");
        }
    }
    let earlier: Vec<&Agent> = flow.stages[..i].iter().flatten().collect();
    if !earlier.is_empty() {
        p += "\n## Results from earlier stages (read what you need)\n";
        for e in earlier {
            p += &format!("- {}: {}\n", e.name, output_path(thread, &e.name).display());
        }
    }
    p += &format!(
        "\n## Flow map\nThe live plan is {}. If the remaining plan must change, you may edit it: add, modify or remove \
         LATER stages only (never this or earlier ones), same JSON schema, and it must stay valid. brigd rereads it before every stage.\n",
        thread_dir(thread).join("flowmap.json").display()
    );
    p += &format!(
        "\n## Output\nWrite your final result to {}. brigd waits for that file; the stage cannot finish without it.\n",
        output_path(thread, &a.name).display()
    );
    p
}

/// Drives one agent to completion: done = output.md exists once it is idle (or exited).
/// One nudge when it goes idle without output. Blocked prompts are left for Mark.
fn supervise(thread: &str, a: &Agent, live: Arc<runner::LiveAgent>, state: &Mutex<State>) -> bool {
    let out = live.dir.join("output.md");
    let nudge = format!(
        "You stopped without writing your result file. If you have not done the task yet, read {} and do it. \
         Then write your final result to {}.",
        live.dir.join("prompt.md").display(),
        out.display()
    );
    let home = PathBuf::from(env::var("HOME").unwrap_or_default());
    let sync = || {
        if let Ok(s) = fs::read_to_string(live.dir.join("session")) {
            let _ = tree::sync_subagents(&tree::subagents_dir(&home, &live.cwd, s.trim()), &live.dir);
        }
    };
    let (mut tick, mut idle_for, mut nudged_at, mut blocked) = (0u32, 0, None::<u32>, false);
    let ok = loop {
        thread::sleep(Duration::from_secs(1));
        tick += 1;
        if tick % 5 == 0 {
            sync();
        }
        let (alive, status) = (live.alive(), live.status());
        if live.dir.join(runner::TERMINATED).exists() {
            log(thread, &format!("{} terminated", a.name));
            break false;
        }
        if out.exists() && (!alive || status == "idle") {
            log(thread, &format!("{} done", a.name));
            break true;
        }
        if !alive {
            log(thread, &format!("{} exited without {}", a.name, out.display()));
            break false;
        }
        if status == "blocked" {
            if !blocked {
                blocked = true;
                let _ = set_agent(state, &a.name, "blocked");
                log(thread, &format!("{} needs you (blocked)", a.name));
            }
            continue;
        }
        if blocked {
            blocked = false;
            let _ = set_agent(state, &a.name, "running");
        }
        // "starting" (e.g. a trust dialog) and "working" just wait. A nudge needs a moment to show as working.
        if status != "idle" || nudged_at.is_some_and(|t| tick - t < 30) {
            idle_for = 0;
            continue;
        }
        idle_for += 1;
        // ponytail: fixed 3s debounce on idle; Stop hooks are exact, the transcript fallback is not.
        if idle_for < 3 {
            continue;
        }
        if nudged_at.is_some() {
            log(thread, &format!("{} stopped without {}", a.name, out.display()));
            break false;
        }
        nudged_at = Some(tick);
        idle_for = 0;
        log(thread, &format!("{} idle without output, nudging", a.name));
        let typed = live.write(nudge.as_bytes()).and_then(|_| {
            thread::sleep(Duration::from_millis(300)); // separate Enter so it is not read as part of a paste
            live.write(b"\r")
        });
        if let Err(e) = typed {
            log(thread, &format!("{}: nudge failed: {e}", a.name));
            break false;
        }
    };
    sync();
    let _ = set_agent(state, &a.name, if ok { "done" } else { "failed" });
    ok
}

// ---------- stray agents: claude typed in a brigd terminal ----------

/// `claude` in a brigd terminal tab runs this (via the ~/.brigd/bin shim). In thread
/// $BRIGD_THREAD it records a new session under <thread>/stray agents/stray-<n>/
/// {cwd,session,base,pid} with the flow agents' hooks, then becomes the real claude
/// (exec keeps the pid). Anything else (no thread, -p, --resume, …) is plain claude.
fn stray(args: &[String]) -> Res<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(real_claude().ok_or("no claude on PATH (outside ~/.brigd/bin)")?);
    let thread = env::var("BRIGD_THREAD").unwrap_or_default();
    if valid_name(&thread, THREAD_MAX) && thread_dir(&thread).is_dir() && !stray_passthrough(args) {
        let setup = || -> Res<[String; 4]> {
            let dir = new_stray_dir(&thread_dir(&thread).join(tree::STRAY))?;
            let cwd = env::current_dir()?;
            fs::write(dir.join("cwd"), cwd.to_string_lossy().as_bytes())?;
            record_base(&cwd, &dir);
            fs::write(dir.join("pid"), std::process::id().to_string())?;
            runner::session_args(&dir)
        };
        match setup() {
            Ok(a) => {
                cmd.args(a);
            }
            Err(e) => eprintln!("brigd: not recording this session: {e}"),
        }
    }
    Err(format!("exec claude: {}", cmd.args(args).exec()).into())
}

/// Args that pick their own session, print, or don't start a chat: no stray, plain claude.
fn stray_passthrough(args: &[String]) -> bool {
    const SUBCOMMANDS: &[&str] = &["mcp", "plugin", "doctor", "update", "install", "setup-token", "config", "auth"];
    const FLAGS: &[&str] = &["-p", "--print", "-r", "--resume", "-c", "--continue", "--session-id", "-v", "--version", "-h", "--help"];
    args.first().is_some_and(|a| SUBCOMMANDS.contains(&a.as_str())) || args.iter().any(|a| FLAGS.contains(&a.split('=').next().unwrap_or(a)))
}

/// Creates and returns <parent>/stray-<n>, the first n not taken (create_dir, so two at once never share one).
fn new_stray_dir(parent: &Path) -> Res<PathBuf> {
    fs::create_dir_all(parent)?;
    for n in 1.. {
        let d = parent.join(format!("stray-{n}"));
        match fs::create_dir(&d) {
            Ok(()) => return Ok(d),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!()
}

/// ~/.brigd/bin: holds the `claude` shim brigd terminals put first on PATH.
fn shim_dir() -> PathBuf {
    root().join("bin")
}

/// The first executable `claude` on PATH outside the shim dir (and not brigd itself).
fn real_claude() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let (shim, me) = (shim_dir(), env::current_exe().ok().and_then(|p| p.canonicalize().ok()));
    let shim_c = shim.canonicalize().ok();
    env::split_paths(&env::var_os("PATH")?)
        .filter(|d| *d != shim && (shim_c.is_none() || d.canonicalize().ok() != shim_c))
        .map(|d| d.join("claude"))
        .find(|c| c.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) && c.canonicalize().ok() != me)
}

/// Writes the `claude` shim and the zsh startup wrappers (an rc that prepends
/// ~/.local/bin would hide the shim; the wrappers re-prepend it after the user's
/// rc). Returns the shim dir.
fn ensure_shim() -> Res<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = shim_dir();
    let shim = "#!/bin/sh\n[ -n \"$BRIGD_BIN\" ] && exec \"$BRIGD_BIN\" stray \"$@\"\n\
                PATH=$(printf %s \"$PATH\" | tr : '\\n' | grep -vxF \"$(dirname \"$0\")\" | paste -sd: -)\nexec claude \"$@\"\n";
    let zenv = "_brigd=$ZDOTDIR; ZDOTDIR=${BRIGD_ZDOTDIR:-$HOME}\n[ -f \"$ZDOTDIR/.zshenv\" ] && . \"$ZDOTDIR/.zshenv\"\nZDOTDIR=$_brigd; unset _brigd\n";
    let zrc = "if [ -n \"$BRIGD_ZDOTDIR\" ]; then ZDOTDIR=$BRIGD_ZDOTDIR; else unset ZDOTDIR; fi\n\
               [ -f \"${ZDOTDIR:-$HOME}/.zshrc\" ] && . \"${ZDOTDIR:-$HOME}/.zshrc\"\nPATH=\"$BRIGD_SHIM:$PATH\"\n";
    let z = root().join("zsh");
    for (path, text) in [(dir.join("claude"), shim), (z.join(".zshenv"), zenv), (z.join(".zshrc"), zrc)] {
        if fs::read_to_string(&path).ok().as_deref() != Some(text) {
            fs::create_dir_all(path.parent().unwrap_or(&dir))?;
            fs::write(&path, text)?;
        }
    }
    fs::set_permissions(dir.join("claude"), fs::Permissions::from_mode(0o755))?;
    Ok(dir)
}

// ---------- ls / status / install-skill ----------

fn ago(t: u64) -> String {
    let d = now().saturating_sub(t);
    match d {
        0..=59 => format!("{d}s ago"),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

fn ls() -> Res<()> {
    let root = thread_dir("");
    let mut rows = Vec::new();
    for entry in fs::read_dir(&root).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok((flow, st)) = load(&name) {
            let total: usize = flow.stages.iter().map(Vec::len).sum();
            let done = st.agents.values().filter(|a| a.status == "done").count();
            let stage = format!("{}/{}", (st.stage + 1).min(flow.stages.len()), flow.stages.len());
            rows.push((st.updated, format!("{name:<13} {:<8} {stage:<6} {done}/{total:<5} {:<9} {}", st.status, ago(st.updated), st.repo)));
        }
    }
    if rows.is_empty() {
        return Ok(println!("no threads. start one: brigd <name> \"task\""));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    println!("{:<13} {:<8} {:<6} {:<7} {:<9} REPO", "NAME", "STATUS", "STAGE", "AGENTS", "UPDATED");
    rows.iter().for_each(|r| println!("{}", r.1));
    Ok(())
}

fn status(name: &str) -> Res<()> {
    let (flow, st) = load(name)?;
    println!("{name}: {} (stage {}/{}), updated {}\nrepo: {}\ngoal: {}", st.status, (st.stage + 1).min(flow.stages.len()), flow.stages.len(), ago(st.updated), st.repo, flow.goal);
    for (i, stage) in flow.stages.iter().enumerate() {
        println!("stage {}", i + 1);
        for a in stage {
            let s = st.agents.get(&a.name).cloned().unwrap_or_default();
            let status = if s.status.is_empty() { "pending" } else { &s.status };
            let out = output_path(name, &a.name);
            let out = if out.exists() { out.display().to_string() } else { "-".into() };
            println!("  {:<19} {:<8} {:<7} wt={:<12} {}", a.name, status, a.model, a.worktree.as_deref().unwrap_or("-"), out);
        }
    }
    Ok(())
}

fn install_skill() -> Res<()> {
    let dir = PathBuf::from(env::var("HOME")?).join(".claude/skills/brigd-plan");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("SKILL.md"), SKILL)?;
    Ok(println!("installed {}", dir.join("SKILL.md").display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str, wt: Option<&str>, mode: &str) -> Agent {
        Agent { name: name.into(), model: "haiku".into(), effort: "low".into(), worktree: wt.map(String::from), task: "t".into(), files: vec![], mode: mode.into() }
    }

    #[test]
    fn validate_rules() {
        let ok = Flow { goal: "g".into(), stages: vec![vec![agent("a", None, "default"), agent("b", Some("x"), "acceptEdits")], vec![agent("c", Some("x"), "acceptEdits")]] };
        assert!(validate(&ok).is_ok());
        let dup = Flow { goal: "g".into(), stages: vec![vec![agent("a", None, "default")], vec![agent("a", None, "default")]] };
        assert!(validate(&dup).unwrap_err().contains("duplicate"));
        let two_writers = Flow { goal: "g".into(), stages: vec![vec![agent("a", Some("x"), "acceptEdits"), agent("b", Some("x"), "auto")]] };
        assert!(validate(&two_writers).unwrap_err().contains("two editing"));
        let crowded = Flow { goal: "g".into(), stages: vec![(0..6).map(|i| agent(&format!("a{i}"), None, "default")).collect()] };
        assert!(validate(&crowded).is_ok()); // no per-space cap without panes
        assert!(validate(&Flow { goal: "g".into(), stages: vec![vec![agent("log", None, "default")]] }).unwrap_err().contains("reserved"));
        assert!(validate(&Flow { goal: "g".into(), stages: vec![vec![]] }).is_err());
        assert!(validate(&Flow { goal: "g".into(), stages: vec![vec![agent("Bad", None, "default")]] }).is_err());
        assert!(validate(&Flow { goal: "g".into(), stages: vec![vec![agent("a", None, "plan")]] }).is_err());
        assert!(!valid_name("a-very-long-thread", THREAD_MAX));
        assert!(RESERVED.contains(&"ls"));
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
        Command::new("chmod").arg("+x").arg(bin.join("claude")).status().unwrap();
        let git = |a: &[&str]| assert!(Command::new("git").arg("-C").arg(&repo).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(a).output().unwrap().status.success());
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "init"]);
        env::set_var("HOME", &t);
        env::set_var("PATH", format!("{}:{}", bin.display(), env::var("PATH").unwrap()));

        let flow = Flow { goal: "g".into(), stages: vec![vec![agent("first", None, "default")], vec![agent("nudge", Some("wt"), "acceptEdits")]] };
        let repo_s = repo.canonicalize().unwrap().to_string_lossy().into_owned();
        fs::create_dir_all(thread_dir("th")).unwrap();
        write_json(&thread_dir("th").join("flowmap.json"), &flow).unwrap();
        let state = State { thread: "th".into(), repo: repo_s.clone(), status: "planned".into(), ..Default::default() };
        execute("th", &flow, state).unwrap();

        let (_, st) = load("th").unwrap();
        assert_eq!(st.status, "done");
        assert!(st.agents.values().all(|a| a.status == "done"));
        let d = thread_dir("th");
        assert_eq!(fs::read_to_string(d.join("first/cwd")).unwrap().trim(), repo_s);
        let wt = t.join(".brigd/worktrees/th/wt");
        assert_eq!(fs::read_to_string(d.join("nudge/cwd")).unwrap().trim(), wt.canonicalize().unwrap().to_string_lossy());
        assert!(fs::read_to_string(d.join("nudge/nudged")).unwrap().starts_with("You stopped without writing"));
        assert!(fs::read_to_string(d.join("nudge/prompt.md")).unwrap().contains(&d.join("first/output.md").display().to_string()));
        assert!(fs::read_to_string(d.join("first/prompt.md")).unwrap().contains(&d.join("flowmap.json").display().to_string()));
        let args = fs::read_to_string(d.join("first/args")).unwrap();
        assert!(args.contains("--session-id\n") && args.contains("--settings\n") && args.contains("--add-dir="));
        assert_eq!(fs::read_to_string(d.join("first/session")).unwrap().len(), 36);
        assert!(fs::read_to_string(d.join("FLOWPLAN")).unwrap().contains("nudging"));
        let _ = fs::remove_dir_all(&t);
    }

    #[test]
    fn stray_names_and_passthrough() {
        let d = env::temp_dir().join(format!("brigd-stray-{}", std::process::id())).join(tree::STRAY);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("stray-1")).unwrap();
        fs::create_dir_all(d.join("stray-3")).unwrap();
        assert_eq!(new_stray_dir(&d).unwrap(), d.join("stray-2"));
        assert_eq!(new_stray_dir(&d).unwrap(), d.join("stray-4"));
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(!stray_passthrough(&v(&[])));
        assert!(!stray_passthrough(&v(&["--model", "opus", "fix the mcp bug"])));
        for a in [&["-p", "hi"][..], &["--resume=abc"], &["-c"], &["--session-id", "x"], &["--version"], &["mcp", "list"], &["--model", "opus", "-h"]] {
            assert!(stray_passthrough(&v(a)), "{a:?}");
        }
        let _ = fs::remove_dir_all(d.parent().unwrap());
    }

    #[test]
    fn background_decision() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|p| p.0 == k).map(|p| p.1.to_string());
        assert!(!background(false, true, env(&[])));
        assert!(background(true, true, env(&[])));
        assert!(background(false, false, env(&[])));
        assert!(background(false, true, env(&[("CLAUDECODE", "1")])));
        assert!(!background(false, true, env(&[("CLAUDECODE", "")])));
        let mac = |mut v: Vec<Surface>| {
            if cfg!(target_os = "macos") {
                v.push(Surface::Terminal);
            }
            v
        };
        const ALL: &[(&str, &str)] = &[("HERDR_ENV", "1"), ("HERDR_WORKSPACE_ID", "w1"), ("TMUX", "/tmp/t,1,0")];
        assert_eq!(surfaces(env(ALL)), mac(vec![Surface::Herdr("w1".into()), Surface::Tmux]));
        assert_eq!(surfaces(env(&[("HERDR_ENV", "1")])), mac(vec![]));
        assert_eq!(surfaces(env(&[])), mac(vec![]));
    }

    #[test]
    fn schema_is_json() {
        serde_json::from_str::<serde_json::Value>(SCHEMA).unwrap();
    }
}
