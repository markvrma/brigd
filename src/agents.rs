//! The coding agents brigd runs (claude, codex, pi, kimi): one `match` per question, so a new
//! agent is one arm in each. The bin is the agent's name. Plus ~/.brigd/config.json, which
//! names the agents on this device and the base agent that plans flows.
//! Non-claude flags come from their docs only (none of them could be run when this was written).

use crate::store::{root, write_json, Agent};
use crate::Res;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::{env, fs};

pub(crate) const AGENTS: &[&str] = &["claude", "codex", "pi", "kimi"];

/// `--model <m>`, or nothing for "default" (the agent's own default, e.g. pi's and kimi's
/// provider-dependent models).
pub(crate) fn model_args(model: &str) -> Vec<String> {
    // ponytail: kimi's long --model is UNVERIFIED (docs show -m).
    match model {
        "default" => vec![],
        m => vec!["--model".into(), m.into()],
    }
}

/// The agent's effort flag, clamped to the levels it has.
pub(crate) fn effort_args(agent: &str, effort: &str) -> Vec<String> {
    match agent {
        "claude" => vec!["--effort".into(), effort.into()],
        // ponytail: codex effort names are UNVERIFIED (minimal..xhigh, model-dependent); max -> high.
        "codex" => {
            let e = if effort == "max" { "high" } else { effort };
            vec!["-c".into(), format!("model_reasoning_effort=\"{e}\"")]
        }
        "pi" => {
            let e = if effort == "max" { "xhigh" } else { effort };
            vec!["--thinking".into(), e.into()]
        }
        // kimi's --thinking is on/off only: effort is ignored.
        _ => vec![],
    }
}

/// argv for a non-claude worker in its closest no-prompt mode, with `writable` (the threads
/// dir) writable; the prompt is last. claude's is flow::claude_flags plus its session args.
pub(crate) fn worker_args(a: &Agent, writable: &Path, prompt: &str) -> Vec<String> {
    let w = writable.display().to_string();
    let mut v = model_args(&a.model);
    v.extend(effort_args(&a.agent, &a.effort));
    let s = |x: &[&str]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match a.agent.as_str() {
        "codex" => v.extend(s(&[
            "--sandbox",
            "workspace-write",
            "--ask-for-approval",
            "never",
            &format!("--add-dir={w}"),
        ])),
        // ponytail: pi has no sandbox and never asks before tool calls; `--` ends its options.
        "pi" => v.push("--".into()),
        // ponytail: kimi has no sandbox (--yolo approves all) and no documented interactive start
        // with a prompt; -p runs it one-shot, so it cannot ask the user. Type the prompt into the PTY if that matters.
        _ => v.extend(s(&["--yolo", &format!("--add-dir={w}"), "-p"])),
    }
    v.push(prompt.into());
    v
}

/// argv for the planner in read-only print mode. claude gets the skill as its system prompt and
/// `schema` as --json-schema; the others get both in the prompt and answer with plain text.
pub(crate) fn planner_args(agent: &str, prompt: &str, skill: &str, schema: &str) -> Vec<String> {
    let s = |x: &[&str]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if agent == "claude" {
        return s(&[
            "-p",
            prompt,
            "--model",
            "opus",
            "--effort",
            "high",
            "--output-format",
            "json",
            "--no-session-persistence",
            "--json-schema",
            schema,
            "--tools",
            "Read,Grep,Glob",
            "--append-system-prompt",
            skill,
            "--permission-mode",
            "auto",
        ]);
    }
    let p = format!(
        "{prompt}\n\nReply with only the flow: one JSON object matching this JSON schema:\n{schema}\n\n\
         Plan it by this skill (brigd-plan):\n\n{skill}"
    );
    let mut v = match agent {
        // ponytail: --skip-git-repo-check is UNVERIFIED; it lets the planner run outside a git repo.
        "codex" => s(&["exec", "--sandbox", "read-only", "--skip-git-repo-check"]),
        // ponytail: pi has no read-only mode; the tool allowlist leaves out bash/edit/write.
        "pi" => s(&["-p", "--tools", "read,grep,find,ls"]),
        // ponytail: kimi's --plan (read-only tools) with --quiet print mode is UNVERIFIED.
        _ => s(&["--quiet", "--plan"]),
    };
    v.extend(effort_args(agent, "high"));
    v.extend(s(if agent == "kimi" { &["-p"] } else { &["--"] }));
    v.push(p);
    v
}

/// Where the agent reads user skills. codex, pi and kimi share ~/.agents/skills.
pub(crate) fn skills_dir(agent: &str, home: &Path) -> PathBuf {
    // ponytail: pi's own ~/.pi/agent/skills is UNVERIFIED; ~/.agents/skills is documented for all three.
    match agent {
        "claude" => home.join(".claude/skills"),
        _ => home.join(".agents/skills"),
    }
}

/// Env vars an agent sets for the commands it runs that describe its session.
fn env_markers(agent: &str) -> &'static [&'static str] {
    match agent {
        // Seen in Claude Code's Bash tool env.
        "claude" => &[
            "CLAUDECODE",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_SESSION_ATTENDED",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "CLAUDE_CODE_EXECPATH",
            "CLAUDE_PID",
        ],
        // ponytail: UNVERIFIED (recalled from codex source); pi and kimi document none. Check `env` inside each.
        "codex" => &["CODEX_SANDBOX", "CODEX_SANDBOX_NETWORK_DISABLED"],
        _ => &[],
    }
}

/// Vars that describe a calling agent session (brigd may run under one): every agent's markers.
pub(crate) fn parent_session_vars() -> impl Iterator<Item = &'static str> {
    AGENTS
        .iter()
        .flat_map(|a| env_markers(a))
        .copied()
        .chain(["AI_AGENT"])
}

/// Is `bin` an executable file on PATH?
pub(crate) fn on_path(bin: &str) -> bool {
    env::var_os("PATH").is_some_and(|p| env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// ~/.brigd/config.json
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub(crate) struct Config {
    /// The agents on this device.
    pub(crate) agents: Vec<String>,
    /// The one that plans flows.
    pub(crate) base: String,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// The config `init` loaded; claude only if it never ran (tests, commands that skip it).
pub(crate) fn config() -> Config {
    CONFIG.get().cloned().unwrap_or_else(|| Config {
        agents: vec!["claude".into()],
        base: "claude".into(),
    })
}

/// Loads ~/.brigd/config.json, or writes it on first run: asks on stdin when `ask`, else
/// keeps the agents found on PATH and claude (else the first found) as base.
pub(crate) fn init(ask: bool) -> Res<()> {
    let path = root().join("config.json");
    let cfg = match fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| format!("{}: {e}", path.display()))?,
        Err(_) => {
            let found: Vec<&str> = AGENTS.iter().copied().filter(|a| on_path(a)).collect();
            if ask {
                eprintln!(
                    "brigd first run: which coding agents to use (delete {} to redo this). Enter keeps [the default].",
                    path.display()
                );
            }
            let read = |q: String| if ask { read_line(&q) } else { String::new() };
            let cfg = setup(&found, &read)?;
            fs::create_dir_all(root())?;
            write_json(&path, &cfg)?;
            cfg
        }
    };
    let _ = CONFIG.set(cfg);
    Ok(())
}

/// The first-run config from the agents `found` on PATH and the user's answers (`read` gets
/// the question, returns the answer; "" keeps the default).
fn setup(found: &[&str], read: &dyn Fn(String) -> String) -> Res<Config> {
    if found.is_empty() {
        return Err(format!("no coding agent on PATH; install one of {AGENTS:?}").into());
    }
    let typed = read(format!(
        "coding agents on this device [{}]: ",
        found.join(",")
    ));
    let agents: Vec<String> = if typed.trim().is_empty() {
        found.iter().map(|s| s.to_string()).collect()
    } else {
        typed
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    if let Some(bad) = agents.iter().find(|a| !AGENTS.contains(&a.as_str())) {
        return Err(format!("unknown agent {bad:?}; supported: {AGENTS:?}").into());
    }
    let first = agents.first().ok_or("no agents given")?;
    let default = if agents.iter().any(|a| a == "claude") {
        "claude"
    } else {
        first
    };
    let base = match read(format!("base agent [{default}]: ")).trim() {
        "" => default.to_string(),
        b => b.to_string(),
    };
    if !agents.contains(&base) {
        return Err(format!("base agent {base:?} is not one of {agents:?}").into());
    }
    Ok(Config { agents, base })
}

/// One trimmed line from stdin after printing `q` (to stderr, like ask_prg).
fn read_line(q: &str) -> String {
    eprint!("{q}");
    let _ = io::stderr().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
    line.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::tests::agent;

    #[test]
    fn effort_clamp_and_argv() {
        assert_eq!(effort_args("claude", "max"), ["--effort", "max"]);
        assert_eq!(
            effort_args("codex", "max"),
            ["-c", "model_reasoning_effort=\"high\""]
        );
        assert_eq!(effort_args("pi", "max"), ["--thinking", "xhigh"]);
        assert_eq!(effort_args("pi", "low"), ["--thinking", "low"]);
        assert!(effort_args("kimi", "high").is_empty());

        let mut a = agent("x", None, "acceptEdits");
        (a.agent, a.model, a.effort) = ("codex".into(), "gpt-5.5".into(), "max".into());
        let w = Path::new("/h/.brigd/threads");
        assert_eq!(
            worker_args(&a, w, "go"),
            [
                "--model",
                "gpt-5.5",
                "-c",
                "model_reasoning_effort=\"high\"",
                "--sandbox",
                "workspace-write",
                "--ask-for-approval",
                "never",
                "--add-dir=/h/.brigd/threads",
                "go"
            ]
        );
        (a.agent, a.model) = ("pi".into(), "default".into());
        assert_eq!(
            worker_args(&a, w, "-go"),
            ["--thinking", "xhigh", "--", "-go"]
        );
        a.agent = "kimi".into();
        assert_eq!(
            worker_args(&a, w, "go"),
            ["--yolo", "--add-dir=/h/.brigd/threads", "-p", "go"]
        );
        let p = planner_args("codex", "plan it", "SKILL", "{}");
        assert_eq!(
            &p[..4],
            ["exec", "--sandbox", "read-only", "--skip-git-repo-check"]
        );
        assert!(p.last().unwrap().starts_with("plan it") && p.last().unwrap().ends_with("SKILL"));
        assert!(planner_args("claude", "x", "s", "{}").contains(&"--json-schema".to_string()));
    }

    #[test]
    fn first_run_setup() {
        let keep = |_: String| String::new();
        let c = setup(&["codex", "pi"], &keep).unwrap();
        assert_eq!(
            (c.agents.join(","), c.base.as_str()),
            ("codex,pi".into(), "codex")
        );
        let c = setup(&["claude", "pi"], &keep).unwrap();
        assert_eq!(c.base, "claude");
        let typed = |q: String| {
            if q.starts_with("coding") {
                "pi, kimi".into()
            } else {
                "kimi".into()
            }
        };
        let c = setup(&["claude"], &typed).unwrap();
        assert_eq!(
            (c.agents, c.base),
            (vec!["pi".into(), "kimi".into()], "kimi".into())
        );
        assert!(setup(&[], &keep).is_err());
        assert!(setup(&["claude"], &|_| "gemini".into()).is_err());
        assert!(
            setup(&["claude", "pi"], &|q: String| if q.starts_with("base") {
                "codex".into()
            } else {
                String::new()
            })
            .is_err()
        );
        let vars: Vec<_> = parent_session_vars().collect();
        assert!(
            vars.contains(&"CLAUDECODE")
                && vars.contains(&"CODEX_SANDBOX")
                && vars.contains(&"AI_AGENT")
        );
    }
}
