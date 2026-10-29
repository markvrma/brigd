---
name: brigd-plan
description: Plan a brigd flow — split a task into stages of parallel coding agents and return the flow JSON that `brigd <name> --flow flow.json` runs. Use when asked to "plan a brigd flow", "make a brigd flow", or "/brigd-plan <task>".
---

# brigd-plan

Turn a task into a **brigd flow**: stages that run one after another, where every
agent inside a stage runs in parallel as its own interactive agent session. brigd
starts stage N+1 only after every agent in stage N has written its result file.

From an agent session you may run `brigd <name> --flow <file>` yourself: it opens in a
new herdr tab, tmux window or Terminal window and returns at once.

## Output contract

Return **only** the flow JSON. No prose, no code fence, no comments.

```
{
  "goal": string,                     // one line: what the whole flow achieves
  "stages": [                         // run in order
    [                                 // one stage: these agents run in parallel
      {
        "name": string,               // ^[a-z][a-z0-9_-]{0,18}$, unique in the flow, not "log"
        "agent": "claude" | "codex" | "pi" | "kimi",  // an enabled agent, see ## Agents
        "model": string,              // a model of that agent, or "default"; see ## Agents
        "effort": "low" | "medium" | "high" | "xhigh" | "max",
        "worktree": string | null,    // ^[a-z][a-z0-9_-]{0,18}$ or null (main repo)
        "task": string,               // self-contained instructions for this agent
        "files": [string],            // repo paths the agent should check first
        "mode": "default" | "acceptEdits" | "auto"
      }
    ]
  ]
}
```

Example, for "find why login is slow and fix it":

```json
{
  "goal": "Find and fix the slow login path",
  "stages": [
    [
      {"name": "trace-auth", "agent": "claude", "model": "sonnet", "effort": "medium", "worktree": null,
       "task": "Read the login request path and list every call that can block, with file:line and why.",
       "files": ["src/auth/login.rs", "src/db/session.rs"], "mode": "default"},
      {"name": "check-db", "agent": "claude", "model": "haiku", "effort": "low", "worktree": null,
       "task": "List the queries the session table gets on login and whether each has an index.",
       "files": ["migrations/"], "mode": "default"}
    ],
    [
      {"name": "fix-login", "agent": "claude", "model": "opus", "effort": "high", "worktree": "login-fix",
       "task": "Using the earlier stage results, fix the slowest blocking call. Keep the diff small. Run the auth tests.",
       "files": ["src/auth/login.rs"], "mode": "acceptEdits"}
    ],
    [
      {"name": "review-fix", "agent": "claude", "model": "sonnet", "effort": "medium", "worktree": "login-fix",
       "task": "Review the uncommitted diff in this worktree for correctness bugs. Report findings only.",
       "files": [], "mode": "default"}
    ]
  ]
}
```

## Splitting rules

- Use the **fewest agents** that cover the task. One agent is a valid flow.
- Agents in one stage must be **independent**: none needs another's output.
- Put work that needs earlier output in a later stage. brigd gives every agent the
  result files of all earlier stages.
- Each `task` must stand alone: the agent sees only its task, its files and earlier
  results, not this conversation.
- An agent may use its own Task/Agent subagents for fan-out inside its task.
- brigd saves the flow as the thread's `flowmap.json` and rereads it before every stage;
  agents may edit its later stages if the remaining plan must change.

## Agents

Use only the agents named in the line `Enabled agents: ...` of your prompt; if there is none, read `~/.brigd/config.json` (`agents`, `base`), and if that is missing use claude only. Pick the
cheapest agent and model that can do the stage; top tier only for planning, hard
debugging, high-stakes edits. Models (cost tier: strengths):

- `claude`: `haiku` cheap (listing, grepping, renaming, formatting); `sonnet` mid
  (standard coding, tests, reviews); `opus` top (planning, hard debugging, high-stakes changes).
- `codex`: a `-mini` model cheap (mechanical edits); `gpt-5.4` mid (standard coding);
  `gpt-5.5` top (hard debugging, planning). Names move fast: use `"default"` when unsure.
- `pi`: `"default"`. Models depend on the user's provider; treat it as that model's tier.
- `kimi`: `"default"`. Cheap long-context coding, strong tool use. It runs one-shot and cannot ask you questions.

`effort` for kimi is ignored; codex clamps `max` to `high`, pi to `xhigh`.
Start `effort` at `low` and raise it only when the task needs real reasoning.

## Worktrees and modes

- Agents with the same `worktree` string share one git worktree, across all stages.
  A different string means a different worktree.
- `null` means the main repo checkout. Use it for read-only work.
- Give a worktree only to agents that **edit code**. A fresh worktree has no build
  artifacts or untracked files (e.g. `.env`).
- One independent change set = one worktree.
- `mode`: brigd launches every agent in its closest no-prompt mode (claude `auto`, codex workspace-write sandbox, pi and kimi auto-approve);
  `mode` only marks which agents edit files. `default` for read-only work,
  `acceptEdits` (or `auto`) for agents that edit files.
- At most **one** `acceptEdits`/`auto` agent per worktree per stage (`null` counts
  as one worktree). Put a second editor of the same worktree in the next stage.

## Paths

`files` must be real paths in the repo. Read the repo (Read, Grep, Glob) before
choosing them. Use `[]` rather than guessing.

## plan-review-go (opt-in)

Only when the user's task **explicitly** asks for it (it names `plan-review-go` or
`/plan-review-go`): return exactly this flow, with `<task>` = the user's task. Never
infer it; otherwise ignore this section and plan as usual.

plan-review-go needs claude's ponytail plugin and a stage that can hold an approval dialog: use claude for stage 1 when it is enabled, and never kimi (it runs one-shot).

```json
{"goal": "<task, one line>", "stages": [
  [{"name": "plan-review", "agent": "<base agent>", "model": "<opus for claude, else that agent's top model>", "effort": "high", "worktree": null,
    "task": "Invoke the plan-review-go skill on this task: <task>. It needs the grill-me skill and the ponytail plugin's ponytail-review skill; if any is missing, say so and stop. Follow it through to the user approving the plan. Then APPEND stages 2..N to this thread's flowmap.json that implement the approved plan, following the brigd-plan splitting, agent/model/effort, worktree, mode and paths rules and keeping the schema valid. Leave stage 1 untouched. Do not implement the plan yourself. Your output.md holds the approved plan plus a short summary of the stages you added.",
    "files": [], "mode": "default"}]
]}
```

This one-stage flow is intentional: brigd rereads flowmap.json after each stage and runs whatever stages the agent appended.
