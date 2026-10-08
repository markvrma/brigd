---
name: brigd-plan
description: Plan a brigd flow — split a task into stages of parallel Claude agents and return the flow JSON that `brigd <name> --flow flow.json` runs. Use when asked to "plan a brigd flow", "make a brigd flow", or "/brigd-plan <task>".
---

# brigd-plan

Turn a task into a **brigd flow**: stages that run one after another, where every
agent inside a stage runs in parallel as its own interactive Claude session. brigd
starts stage N+1 only after every agent in stage N has written its result file.

From Claude Code you may run `brigd <name> --flow <file>` yourself: it opens in a
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
        "model": "haiku" | "sonnet" | "opus",
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
      {"name": "trace-auth", "model": "sonnet", "effort": "medium", "worktree": null,
       "task": "Read the login request path and list every call that can block, with file:line and why.",
       "files": ["src/auth/login.rs", "src/db/session.rs"], "mode": "default"},
      {"name": "check-db", "model": "haiku", "effort": "low", "worktree": null,
       "task": "List the queries the session table gets on login and whether each has an index.",
       "files": ["migrations/"], "mode": "default"}
    ],
    [
      {"name": "fix-login", "model": "opus", "effort": "high", "worktree": "login-fix",
       "task": "Using the earlier stage results, fix the slowest blocking call. Keep the diff small. Run the auth tests.",
       "files": ["src/auth/login.rs"], "mode": "acceptEdits"}
    ],
    [
      {"name": "review-fix", "model": "sonnet", "effort": "medium", "worktree": "login-fix",
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

## Model and effort

- `haiku`: mechanical work (listing, grepping, renaming, formatting).
- `sonnet`: standard coding, tests, reviews.
- `opus`: planning, hard debugging, high-stakes changes.
- Start `effort` at `low` and raise it only when the task needs real reasoning.

## Worktrees and modes

- Agents with the same `worktree` string share one git worktree, across all stages.
  A different string means a different worktree.
- `null` means the main repo checkout. Use it for read-only work.
- Give a worktree only to agents that **edit code**. A fresh worktree has no build
  artifacts or untracked files (e.g. `.env`).
- One independent change set = one worktree.
- `mode`: brigd launches every agent in Claude Code's `auto` permission mode;
  `mode` only marks which agents edit files. `default` for read-only work,
  `acceptEdits` (or `auto`) for agents that edit files.
- At most **one** `acceptEdits`/`auto` agent per worktree per stage (`null` counts
  as one worktree). Put a second editor of the same worktree in the next stage.

## Paths

`files` must be real paths in the repo. Read the repo (Read, Grep, Glob) before
choosing them. Use `[]` rather than guessing.
