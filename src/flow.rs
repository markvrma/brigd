//! The flow definition: its JSON schema and limits, validation, the planner call that writes
//! one, and the text derived from it (FLOWPLAN, each agent's prompt.md and claude flags).

use crate::launch::PARENT_SESSION_VARS;
use crate::store::{output_path, thread_dir, Agent, Flow, Status};
use crate::{tui, Res};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::Command;

pub(crate) const SKILL: &str = include_str!("../skills/brigd-plan/SKILL.md");
pub(crate) const RESERVED: &[&str] = &["ls", "status", "resume", "install-skill", "help", "stray"];
const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const MODES: &[&str] = &["default", "acceptEdits", "auto"];
const WRITE_MODES: &[&str] = &["acceptEdits", "auto"];
// Keeps "<thread>/<agent>" labels short enough for a sidebar.
pub(crate) const THREAD_MAX: usize = 12;
const AGENT_MAX: usize = 19;

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

pub(crate) fn valid_name(s: &str, max: usize) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= max
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

/// Checks what the schema cannot: at least one stage, no empty stage, unique valid agent
/// names (not "log"), known effort and mode, valid worktree keys, and at most one editing
/// agent (WRITE_MODES) per space per stage.
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
                return Err(format!(
                    "{who}: name must be [a-z][a-z0-9_-], max {AGENT_MAX}"
                ));
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
                return Err(format!(
                    "{who}: worktree must be null or [a-z][a-z0-9_-], max {AGENT_MAX}"
                ));
            }
            let c = editors.entry(wt).or_default();
            *c += WRITE_MODES.contains(&a.mode.as_str()) as usize;
            let space = if wt.is_empty() {
                "main repo".into()
            } else {
                format!("worktree {wt:?}")
            };
            if *c > 1 {
                return Err(format!("stage {n}: two editing agents ({WRITE_MODES:?}) in {space}; move one to the next stage"));
            }
        }
    }
    Ok(())
}

impl Flow {
    pub(crate) fn agent(&self, name: &str) -> Option<&Agent> {
        self.stages.iter().flatten().find(|a| a.name == name)
    }
}

/// A flowmap.json's text as a valid Flow; the error is the parse or validation message.
pub(crate) fn flow_from_json(text: &str) -> Result<Flow, String> {
    let flow: Flow = serde_json::from_str(text).map_err(|e| e.to_string())?;
    validate(&flow)?;
    Ok(flow)
}

/// Reads, parses and validates a flow file.
pub(crate) fn read_flow(path: &Path) -> Result<Flow, String> {
    flow_from_json(&fs::read_to_string(path).map_err(|e| e.to_string())?)
}

/// Runs the planner in `repo` and returns a valid flow. Never prints: the TUI calls it too.
pub(crate) fn plan(task: &str, repo: &Path) -> Res<Flow> {
    let mut prompt = format!("Plan a brigd flow for this task:\n\n{task}");
    for attempt in 0..2 {
        // ponytail: skill text passed as system prompt so it works uninstalled; install-skill is for interactive /brigd-plan.
        let mut cmd = Command::new("claude");
        // brigd may run under Claude Code; the planner is a session of its own.
        PARENT_SESSION_VARS
            .iter()
            .for_each(|v| _ = cmd.env_remove(v));
        let out = cmd
            .current_dir(repo)
            .args([
                "-p",
                &prompt,
                "--model",
                "opus",
                "--effort",
                "high",
                "--output-format",
                "json",
            ])
            .args([
                "--no-session-persistence",
                "--json-schema",
                SCHEMA,
                "--tools",
                "Read,Grep,Glob",
            ])
            .args(["--append-system-prompt", SKILL, "--permission-mode", "auto"])
            .output()?;
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| {
            format!(
                "planner output not JSON ({e}): {}",
                String::from_utf8_lossy(&out.stderr)
            )
        })?;
        let so = v
            .get("structured_output")
            .cloned()
            .ok_or_else(|| format!("planner returned no structured_output: {v}"))?;
        let flow: Flow = serde_json::from_value(so.clone())?;
        match validate(&flow) {
            Ok(()) => return Ok(flow),
            Err(e) if attempt == 0 => prompt += &format!("\n\nYour previous flow was invalid: {e}\nPrevious flow: {so}\nReturn a fixed flow."),
            Err(e) => return Err(format!("planner flow still invalid: {e}\n{}", serde_json::to_string_pretty(&so)?).into()),
        }
    }
    unreachable!()
}

pub(crate) fn print_flow(flow: &Flow) {
    println!("\n{}", flow_text(flow));
}

/// The last line of the plan in FLOWPLAN; the run log follows it.
pub(crate) const LOG_MARK: &str = "└  log";

/// The TUI's FLOWPLAN tree (tui::plan_lines) as plain text, every step pending, ending at LOG_MARK.
pub(crate) fn flow_text(flow: &Flow) -> String {
    tui::plan_lines(flow, |_| Status::Pending, |_| true, &mut vec![], 0, 100)
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                + "\n"
        })
        .collect()
}

/// The prompt.md for agent `a` in stage `i`: the thread goal, its task and files, the
/// output.md paths of every earlier agent, the live flowmap.json, and where to write its result.
pub(crate) fn task_prompt(thread: &str, flow: &Flow, i: usize, a: &Agent) -> String {
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
        "\n## Output\nWrite your final result to {}. brigd waits for that file; the stage cannot finish without it. \
         End it with a `## Files changed` section: one `- /absolute/path` line per file you created, edited or deleted, by any means \
         (Edit/Write, or a script or shell command). brigd shows exactly those as this agent's diffs.\n",
        output_path(thread, &a.name).display()
    );
    p
}

/// claude flags for an agent, minus the session/hooks/prompt the runner adds.
/// Variadic options use --flag=value so they cannot swallow the trailing prompt.
pub(crate) fn claude_flags(thread: &str, a: &Agent) -> Vec<String> {
    let dir = thread_dir(thread);
    let sys = format!(
        "BRIGD WORKER {thread}/{}: do the task. You may use the Task/Agent tool for subagents, \
         but never run the `claude` CLI or start other claude sessions. \
         When finished, first write {} holding only a ```mermaid code block: one small \
         `flowchart TD` of the steps you took (at most 10 nodes like A[short label], \
         edges only `-->` or `-->|label|`, no styling or subgraphs). \
         Then write your final result to {}. \
         COMPLETION: write flow.md and output.md only when the task is fully done. If you cannot \
         complete it for any reason (missing file/skill/tool/access, failed precondition, an earlier \
         stage's result missing or incomplete, ambiguity that materially changes the result), do NOT \
         write either file; ask the user with AskUserQuestion offering 2-4 concrete ways forward (the \
         user can pick Other to reply in free text and discuss). Never end your turn to wait for a \
         reply; ask follow-ups with AskUserQuestion. If the user says stop or abort, end your turn \
         without writing them.",
        a.name,
        dir.join(&a.name).join("flow.md").display(),
        dir.join(&a.name).join("output.md").display()
    );
    let s = |x: &str| x.to_string();
    vec![
        s("--model"),
        a.model.clone(),
        s("--effort"),
        a.effort.clone(),
        s("--permission-mode"),
        s("auto"), // every brigd agent runs in auto mode; `mode` only marks editors
        s("-n"),
        a.name.clone(),
        // The thread dir sits outside the agent's cwd; grant it so reading the task and writing the result never prompt.
        format!("--add-dir={}", dir.display()),
        // Edit rules cover every file-writing tool; claude rejects Write(path) rules.
        s("--allowedTools=Read(~/.brigd/threads/**),Edit(~/.brigd/threads/**)"),
        s("--append-system-prompt"),
        sys,
    ]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn agent(name: &str, wt: Option<&str>, mode: &str) -> Agent {
        Agent {
            name: name.into(),
            model: "haiku".into(),
            effort: "low".into(),
            worktree: wt.map(String::from),
            task: "t".into(),
            files: vec![],
            mode: mode.into(),
        }
    }

    #[test]
    fn validate_rules() {
        let ok = Flow {
            goal: "g".into(),
            stages: vec![
                vec![
                    agent("a", None, "default"),
                    agent("b", Some("x"), "acceptEdits"),
                ],
                vec![agent("c", Some("x"), "acceptEdits")],
            ],
        };
        assert!(validate(&ok).is_ok());
        let dup = Flow {
            goal: "g".into(),
            stages: vec![
                vec![agent("a", None, "default")],
                vec![agent("a", None, "default")],
            ],
        };
        assert!(validate(&dup).unwrap_err().contains("duplicate"));
        let two_writers = Flow {
            goal: "g".into(),
            stages: vec![vec![
                agent("a", Some("x"), "acceptEdits"),
                agent("b", Some("x"), "auto"),
            ]],
        };
        assert!(validate(&two_writers).unwrap_err().contains("two editing"));
        let crowded = Flow {
            goal: "g".into(),
            stages: vec![(0..6)
                .map(|i| agent(&format!("a{i}"), None, "default"))
                .collect()],
        };
        assert!(validate(&crowded).is_ok()); // no per-space cap without panes
        assert!(validate(&Flow {
            goal: "g".into(),
            stages: vec![vec![agent("log", None, "default")]]
        })
        .unwrap_err()
        .contains("reserved"));
        assert!(validate(&Flow {
            goal: "g".into(),
            stages: vec![vec![]]
        })
        .is_err());
        assert!(validate(&Flow {
            goal: "g".into(),
            stages: vec![vec![agent("Bad", None, "default")]]
        })
        .is_err());
        assert!(validate(&Flow {
            goal: "g".into(),
            stages: vec![vec![agent("a", None, "plan")]]
        })
        .is_err());
        assert!(!valid_name("a-very-long-thread", THREAD_MAX));
        assert!(RESERVED.contains(&"ls"));
    }

    #[test]
    fn schema_is_json() {
        serde_json::from_str::<serde_json::Value>(SCHEMA).unwrap();
    }
}
