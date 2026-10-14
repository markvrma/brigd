---
name: brigd-resume
description: Continue a finished or stopped brigd thread with new stages. Appends stages to the thread's flowmap.json, reopens the thread, and runs `brigd resume <name> --bg` so an open brigd TUI or a new terminal picks them up. Use when asked to "resume a brigd thread", "add a stage to a brigd flow", "continue the flow", or "/brigd-resume <thread> <what to add>".
---

# brigd-resume

Extend an existing brigd thread (`~/.brigd/threads/<name>/`) with new stages, then make
`brigd resume <name>` accept it. brigd refuses to resume a thread whose `state.json` says
`"status": "done"`, and editing `flowmap.json` alone does not change that.

## Steps

1. **Find the thread.** Read `~/.brigd/threads/<name>/flowmap.json`, `state.json` and the
   `<agent>/output.md` files you need. Never edit earlier stages; only append.
2. **Write the new stages** as extra entries in `stages`, using the same schema and rules as
   the `brigd-plan` skill (unique names `^[a-z][a-z0-9_-]{0,18}$`, parallel agents
   independent, at most 4 agents per worktree per stage, at most one `acceptEdits`/`auto`
   agent per worktree per stage). Each `task` must stand alone. Tell the agent to read the
   earlier results by absolute path (`~/.brigd/threads/<name>/<agent>/output.md`), to write
   its own result to `~/.brigd/threads/<name>/<agent-name>/output.md`, and to end it with a
   `## Files changed` section. Reuse an existing worktree string to continue in the same
   worktree. Update `goal` only if the user asks.
3. **Validate**: the file must still parse as JSON, and the stage count must have grown.
4. **Reopen the thread.** Back up `state.json` (to the scratchpad), then set the top-level
   `"status"` to `"stopped"`. Leave per-agent entries and `"stage"` alone.
   `FLOWPLAN` is only a display snapshot; do not edit it.
5. **Resume it.** Run `brigd resume <name> --bg` yourself with the Bash tool. If a brigd TUI
   is open, the command returns at once and that TUI runs the new stages. Otherwise it opens
   a new herdr tab, tmux window or Terminal window running the thread. Report the one line it
   printed. Give the backup path for undo. Warn once that `state.json` `"stage"` is brigd's
   own counter: if resume reruns the last stage or does nothing, try `"stage"` +/-1, or fall
   back to a one-stage flow file run as a new thread with `brigd <name>-2 --flow <file>`.

## Rules

- Do not edit code or run the agents yourself. Running `brigd resume <name> --bg` is the
  only brigd command you run.
- Do not touch earlier stages in `flowmap.json`.
- Reply briefly: which stages were added, the backup path, and the line `brigd resume --bg` printed.
