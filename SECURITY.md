# Security policy

## Reporting a vulnerability

Please report security issues privately through GitHub: **Security → Report a
vulnerability** on this repository. Don't open a public issue for them.

I'll acknowledge a report within a few days and keep you updated until it's fixed.

## Scope and what auto permission mode implies

brigd launches **every** agent it spawns with `claude --permission-mode auto`.
In auto mode Claude Code runs tools (shell commands, file edits) without asking
you first, so an agent works unattended inside its stage. That means:

- An agent can read and write anything your user account can, in the repo or
  worktree it runs in and beyond, and can run shell commands and use the network.
- A hostile prompt, file, web page or dependency that an agent reads can steer it
  into doing those things (prompt injection). brigd adds no sandbox on top of
  Claude Code's own safeguards.
- Only run flows you wrote or reviewed, in repositories you trust. Use a
  disposable checkout or VM for untrusted code.
- Agents with a `worktree` key work in their own git worktree and branch
  (`brigd/<thread>/<key>`), which limits accidental overlap but is not isolation.

Running agents in auto mode is the intended behaviour, so it is not a
vulnerability by itself. In scope: ways brigd could make that worse than
documented, such as running an agent outside its configured worktree, writing
or deleting files outside the thread directories, `Delete` removing more than
the thread's own worktrees and `brigd/` branches, or flow/thread names that
escape their directory. Also in scope: `setup.sh`.

Only the latest release is supported.
