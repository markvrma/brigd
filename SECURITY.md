# Security policy

## Reporting a vulnerability

Please report security issues privately through GitHub: **Security → Report a
vulnerability** on this repository. Don't open a public issue for them.

I'll acknowledge a report within a few days and keep you updated until it's fixed.

## Scope and what unattended agent modes imply

brigd launches **every** agent it spawns in a no-prompt mode, so it works
unattended inside its stage. The mode depends on the agent:

- claude: `claude --permission-mode auto`. Claude Code runs tools (shell commands,
  file edits) without asking first.
- codex: `codex --sandbox workspace-write --ask-for-approval never`. Codex's own
  sandbox limits writes; the network and paths outside it follow codex's settings.
- pi and kimi: no sandbox. pi never asks before a tool call; kimi runs with `--yolo`,
  which approves every tool call.

Outside codex's sandbox, an agent:

- Can read and write anything your user account can, in the repo or
  worktree it runs in and beyond, and can run shell commands and use the network.
- A hostile prompt, file, web page or dependency that an agent reads can steer it
  into doing those things (prompt injection). brigd adds no sandbox on top of
  the agent CLIs' own safeguards.
- Only run flows you wrote or reviewed, in repositories you trust. Use a
  disposable checkout or VM for untrusted code.
- Agents with a `worktree` key work in their own git worktree and branch
  (`brigd/<thread>/<key>`), which limits accidental overlap but is not isolation.

Running agents unattended is the intended behaviour, so it is not a
vulnerability by itself. In scope: ways brigd could make that worse than
documented, such as running an agent outside its configured worktree, writing
or deleting files outside the thread directories, `Delete` removing more than
the thread's own worktrees and `brigd/` branches, or flow/thread names that
escape their directory. Also in scope: `setup.sh`.

Only the latest release is supported.
