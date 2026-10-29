//! Starting `brigd resume` somewhere else: a new herdr tab, tmux window or Terminal window,
//! or the already open TUI (tui.pid) when `--bg` is asked for.

use crate::flow::PARENT_SESSION_VARS;
use crate::runner::{self, sh_quote};
use crate::store::root;
use crate::Res;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;
use std::{env, fs, thread};

#[derive(Debug, PartialEq)]
pub(crate) enum ResumeMode {
    /// Take over this terminal with the TUI.
    Here,
    /// Mark the thread "queued" for the open TUI to claim.
    Queue,
    /// Open a new herdr tab / tmux window / Terminal window running `brigd resume`.
    Launch,
}

/// How `brigd resume` runs a thread: here without `--bg`; with it, by the open TUI if one
/// is alive, else in a new terminal.
pub(crate) fn resume_how(bg: bool, tui: bool) -> ResumeMode {
    match (bg, tui) {
        (false, _) => ResumeMode::Here,
        (true, true) => ResumeMode::Queue,
        (true, false) => ResumeMode::Launch,
    }
}

/// The open TUI that claims "queued" threads writes its pid here.
pub(crate) fn tui_pid_path() -> PathBuf {
    root().join("tui.pid")
}

pub(crate) fn tui_pid() -> Option<u32> {
    fs::read_to_string(tui_pid_path()).ok()?.trim().parse().ok()
}

pub(crate) fn tui_alive() -> bool {
    tui_pid().is_some_and(runner::pid_alive)
}

/// True when nobody can answer the y/N confirm or watch a fullscreen TUI here: `--bg`,
/// no TTY on stdin/stdout, or Claude Code's Bash tool (it sets CLAUDECODE=1).
pub(crate) fn background(bg_flag: bool, tty: bool, var: impl Fn(&str) -> Option<String>) -> bool {
    bg_flag || !tty || var("CLAUDECODE").is_some_and(|v| !v.is_empty())
}

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
pub(crate) fn launch_bg(name: &str, repo: &str) -> Res<String> {
    // A tmux window inherits this process's env: drop the calling Claude session's vars so agents start clean.
    let unset: String = PARENT_SESSION_VARS
        .iter()
        .map(|v| format!("-u {v} "))
        .collect();
    let cmd = format!(
        "env {unset}{} resume {name}",
        sh_quote(&env::current_exe()?.to_string_lossy())
    );
    let label = format!("brigd-{name}");
    let mut errs = vec![];
    for s in surfaces(|k| env::var(k).ok()) {
        let r = match &s {
            Surface::Herdr(ws) => herdr_tab(ws, repo, &label, &cmd),
            // `|| read` keeps the window open on an error so it can be read.
            Surface::Tmux => run_ok(Command::new("tmux").args([
                "new-window",
                "-d",
                "-n",
                &label,
                "-c",
                repo,
                &format!("{cmd} || read _"),
            ]))
            .map(|_| format!("tmux window {label} (tmux select-window -t {label})")),
            Surface::Terminal => {
                let sh = format!("cd {} && {cmd}", sh_quote(repo));
                let script = format!(
                    "tell application \"Terminal\" to do script \"{}\"",
                    sh.replace('\\', "\\\\").replace('"', "\\\"")
                );
                run_ok(Command::new("osascript").args(["-e", &script]))
                    .map(|_| "a new Terminal window".to_string())
            }
        };
        match r {
            Ok(at) => return Ok(at),
            Err(e) => errs.push(format!("{s:?}: {e}")),
        }
    }
    let why = if errs.is_empty() {
        "not in herdr or tmux, not on macOS".into()
    } else {
        errs.join("; ")
    };
    Err(format!("no terminal to run thread {name} in ({why}); it is saved, `brigd resume {name}` in a terminal runs it").into())
}

/// Opens an unfocused herdr tab in workspace `ws` and runs `cmd` in its shell.
fn herdr_tab(ws: &str, repo: &str, label: &str, cmd: &str) -> Res<String> {
    let out = run_ok(Command::new("herdr").args([
        "tab",
        "create",
        "--workspace",
        ws,
        "--cwd",
        repo,
        "--label",
        label,
        "--no-focus",
    ]))?;
    let v: serde_json::Value = serde_json::from_str(&out)?;
    let pane = v["result"]["root_pane"]["pane_id"]
        .as_str()
        .ok_or_else(|| format!("no root pane in {out}"))?;
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
        return Err(String::from_utf8_lossy(&out.stderr)
            .trim()
            .to_string()
            .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_decision() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|p| p.0 == k).map(|p| p.1.to_string())
        };
        assert!(!background(false, true, env(&[])));
        assert!(background(true, true, env(&[])));
        assert!(background(false, false, env(&[])));
        assert!(background(false, true, env(&[("CLAUDECODE", "1")])));
        assert!(!background(false, true, env(&[("CLAUDECODE", "")])));
        assert_eq!(resume_how(false, true), ResumeMode::Here);
        assert_eq!(resume_how(true, true), ResumeMode::Queue);
        assert_eq!(resume_how(true, false), ResumeMode::Launch);
        let mac = |mut v: Vec<Surface>| {
            if cfg!(target_os = "macos") {
                v.push(Surface::Terminal);
            }
            v
        };
        const ALL: &[(&str, &str)] = &[
            ("HERDR_ENV", "1"),
            ("HERDR_WORKSPACE_ID", "w1"),
            ("TMUX", "/tmp/t,1,0"),
        ];
        assert_eq!(
            surfaces(env(ALL)),
            mac(vec![Surface::Herdr("w1".into()), Surface::Tmux])
        );
        assert_eq!(surfaces(env(&[("HERDR_ENV", "1")])), mac(vec![]));
        assert_eq!(surfaces(env(&[])), mac(vec![]));
    }
}
