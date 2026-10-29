//! The flow definition: its JSON schema and limits, validation, the planner call that writes
//! one, and the text derived from it (each agent's prompt.md and claude flags; FLOWPLAN's is
//! tui::flow_text).

use crate::agents::{self, AGENTS};
use crate::store::{output_path, thread_dir, valid_name, Agent, Flow};
use crate::Res;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::Command;

pub(crate) const SKILL: &str = include_str!("../skills/brigd-plan/SKILL.md");
const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const MODES: &[&str] = &["default", "acceptEdits", "auto"];
const WRITE_MODES: &[&str] = &["acceptEdits", "auto"];
// Keeps "<thread>/<agent>" labels short enough for a sidebar (with store::THREAD_MAX).
const AGENT_MAX: usize = 19;

const SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["goal","stages"],
"properties":{"goal":{"type":"string"},
"stages":{"type":"array","minItems":1,"items":{"type":"array","minItems":1,"items":{
"type":"object","additionalProperties":false,
"required":["name","agent","model","effort","worktree","task","files","mode"],
"properties":{
"name":{"type":"string","pattern":"^[a-z][a-z0-9_-]{0,18}$"},
"agent":{"type":"string","enum":["claude","codex","pi","kimi"]},
"model":{"type":"string"},
"effort":{"type":"string","enum":["low","medium","high","xhigh","max"]},
"worktree":{"type":["string","null"]},
"task":{"type":"string"},
"files":{"type":"array","items":{"type":"string"}},
"mode":{"type":"string","enum":["default","acceptEdits","auto"]}}}}}}}"#;

/// Checks what the schema cannot: at least one stage, no empty stage, unique valid agent
/// names (not "log"), a known agent, effort and mode, valid worktree keys, and at most one editing
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
            if !AGENTS.contains(&a.agent.as_str()) {
                return Err(format!("{who}: agent must be one of {AGENTS:?}"));
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

/// Runs the planner (the base agent, read-only) in `repo` and returns a valid flow. Never
/// prints: the TUI calls it too.
pub(crate) fn plan(task: &str, repo: &Path) -> Res<Flow> {
    let cfg = agents::config();
    let base = cfg.base.as_str();
    let mut prompt = format!(
        "Plan a brigd flow for this task:\n\n{task}\n\nEnabled agents: {}. Base: {base}.",
        cfg.agents.join(", ")
    );
    for attempt in 0..2 {
        // ponytail: skill text passed in the planner call so it works uninstalled; install-skill is for interactive /brigd-plan.
        let mut cmd = Command::new(base);
        // brigd may run under an agent session; the planner is a session of its own.
        agents::parent_session_vars().for_each(|v| _ = cmd.env_remove(v));
        let out = cmd
            .current_dir(repo)
            .args(agents::planner_args(base, &prompt, SKILL, SCHEMA))
            .output()?;
        let flow = planner_json(base, &out.stdout)
            .map_err(|e| format!("{e}: {}", String::from_utf8_lossy(&out.stderr).trim()))
            .and_then(|so| {
                let f = serde_json::from_value::<Flow>(so.clone()).map_err(|e| e.to_string());
                f.and_then(|f| validate(&f).map(|()| f))
                    .map_err(|e| format!("{e}\nPrevious flow: {so}"))
            });
        match flow {
            Ok(flow) => return Ok(flow),
            Err(e) if attempt == 0 => {
                prompt += &format!("\n\nYour previous flow was invalid: {e}\nReturn a fixed flow.")
            }
            Err(e) => return Err(format!("planner flow still invalid: {e}").into()),
        }
    }
    unreachable!()
}

/// The flow JSON in the planner's stdout: claude's `structured_output`, else the text from
/// the first '{' to the last '}'.
fn planner_json(base: &str, stdout: &[u8]) -> Result<serde_json::Value, String> {
    let s = String::from_utf8_lossy(stdout);
    if base == "claude" {
        let v: serde_json::Value =
            serde_json::from_str(&s).map_err(|e| format!("planner output not JSON ({e})"))?;
        return v
            .get("structured_output")
            .cloned()
            .ok_or_else(|| format!("planner returned no structured_output: {v}"));
    }
    let (Some(i), Some(j)) = (s.find('{'), s.rfind('}')) else {
        return Err(format!("planner output has no JSON object: {s}"));
    };
    serde_json::from_str(&s[i..=j.max(i)]).map_err(|e| format!("planner output not JSON ({e})"))
}

/// The last line of the plan in FLOWPLAN; the run log follows it.
pub(crate) const LOG_MARK: &str = "└  log";

/// The prompt.md for agent `a` in stage `i`: a non-claude agent's worker rules, the thread goal, its task and files, the
/// output.md paths of every earlier agent, the live flowmap.json, and where to write its result.
pub(crate) fn task_prompt(thread: &str, flow: &Flow, i: usize, a: &Agent) -> String {
    let mut p = if a.agent == "claude" {
        String::new()
    } else {
        format!("{}\n\n", worker_rules(thread, a))
    };
    p += &format!(
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

/// The BRIGD WORKER rules: do the task, never start other agents, write flow.md then
/// output.md only when done, else ask the user. claude gets them as its system prompt (asking
/// with AskUserQuestion, which its hooks report as blocked); the others at the top of prompt.md.
fn worker_rules(thread: &str, a: &Agent) -> String {
    let dir = thread_dir(thread).join(&a.name);
    let (sub, ask) = if a.agent == "claude" {
        (
            "You may use the Task/Agent tool for subagents, but never",
            "ask the user with AskUserQuestion offering 2-4 concrete ways forward (the \
             user can pick Other to reply in free text and discuss). Never end your turn to wait for a \
             reply; ask follow-ups with AskUserQuestion.",
        )
    } else {
        (
            "Never",
            "ask the user, offering 2-4 concrete ways forward, and wait for the reply.",
        )
    };
    format!(
        "BRIGD WORKER {thread}/{}: do the task. {sub} run the `claude`, `codex`, `pi` or `kimi` CLI or start other agent sessions \
         (run `brigd` only when your task says to). \
         When finished, first write {} holding only a ```mermaid code block: one small \
         `flowchart TD` of the steps you took (at most 10 nodes like A[short label], \
         edges only `-->` or `-->|label|`, no styling or subgraphs). \
         Then write your final result to {}. \
         COMPLETION: write flow.md and output.md only when the task is fully done. If you cannot \
         complete it for any reason (missing file/skill/tool/access, failed precondition, an earlier \
         stage's result missing or incomplete, ambiguity that materially changes the result), do NOT \
         write either file; {ask} If the user says stop or abort, end your turn \
         without writing them.",
        a.name,
        dir.join("flow.md").display(),
        dir.join("output.md").display()
    )
}

/// claude flags for an agent, minus the session/hooks/prompt the runner adds.
/// Variadic options use --flag=value so they cannot swallow the trailing prompt.
pub(crate) fn claude_flags(thread: &str, a: &Agent) -> Vec<String> {
    let dir = thread_dir(thread);
    let s = |x: &str| x.to_string();
    let mut v = agents::model_args(&a.model);
    v.extend(agents::effort_args("claude", &a.effort));
    v.extend([
        s("--permission-mode"),
        s("auto"), // every brigd agent runs in auto mode; `mode` only marks editors
        s("-n"),
        a.name.clone(),
        // The thread dir sits outside the agent's cwd; grant it so reading the task and writing the result never prompt.
        format!("--add-dir={}", dir.display()),
        // Edit rules cover every file-writing tool; claude rejects Write(path) rules.
        s("--allowedTools=Read(~/.brigd/threads/**),Edit(~/.brigd/threads/**)"),
        s("--append-system-prompt"),
        worker_rules(thread, a),
    ]);
    v
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn agent(name: &str, wt: Option<&str>, mode: &str) -> Agent {
        Agent {
            name: name.into(),
            agent: "claude".into(),
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
    }

    #[test]
    fn schema_is_json() {
        serde_json::from_str::<serde_json::Value>(SCHEMA).unwrap();
    }

    #[test]
    fn planner_json_shapes() {
        let v = planner_json(
            "pi",
            b"Here it is:\n```json\n{\"goal\":\"g\",\"x\":{}}\n```",
        )
        .unwrap();
        assert_eq!(v["goal"], "g");
        assert!(planner_json("codex", b"no flow").is_err());
        let v = planner_json("claude", br#"{"structured_output":{"goal":"c"}}"#).unwrap();
        assert_eq!(v["goal"], "c");
        let old: Agent =
            serde_json::from_str(r#"{"name":"a","model":"m","effort":"low","task":"t"}"#).unwrap();
        assert_eq!(old.agent, "claude"); // flowmaps from before the agent field
        let mut a = agent("a", None, "default");
        a.agent = "gemini".into();
        let bad = Flow {
            goal: "g".into(),
            stages: vec![vec![a]],
        };
        assert!(validate(&bad).unwrap_err().contains("agent must be"));
    }
}
