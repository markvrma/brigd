# brigd

brigd plans a task as a staged flow of Claude Code agents and runs each agent in its own PTY,
inside one TUI. Stages run in order; agents within a stage run in parallel.

> **Unofficial.** brigd is an independent project, not affiliated with, endorsed by, or sponsored by Anthropic.
> "Claude" and "Claude Code" are trademarks of Anthropic.

## Requirements

- [Claude Code](https://docs.claude.com/en/docs/claude-code) CLI (`claude`), installed and logged in. Claude is the default agent.
- Rust 1.88 or newer (`cargo`), and `git`.
- Optional: `tmux` or `herdr`. `--bg` opens the TUI in a new herdr tab or tmux window; otherwise it falls back to Terminal.app on macOS.

## Install

```sh
cargo install brigd --locked
brigd install-skill
```

From source: `cargo install --git https://github.com/markvrma/brigd --locked`, or see Quick setup below.
Make sure `~/.cargo/bin` is on your `PATH`.

## Quick setup

One line (clones to `~/brigd`, or `$BRIGD_DIR`):

```sh
curl -fsSL https://raw.githubusercontent.com/markvrma/brigd/main/setup.sh | bash
```

Or from a clone:

```sh
git clone https://github.com/markvrma/brigd.git && cd brigd && ./setup.sh
```

Manual equivalent:

```sh
cargo install --path . --locked
brigd install-skill
```

Make sure `~/.cargo/bin` is on your `PATH`.

## Skills

`brigd install-skill` installs **brigd-plan**, **plan-review-go**, **grill-me** and **grilling** to `~/.claude/skills/`.
In Claude Code, `/brigd-plan <task>` splits a task into stages of parallel agents and returns a
flow JSON that `brigd <name> --flow flow.json` runs.

`brigd <name> "... use plan-review-go"` opts into a review path: stage 1 is an agent that runs plan-review-go with
you, then rewrites the later stages. It needs the **plan-review-go** and **grill-me** skills (installed by `brigd install-skill`) and the
**ponytail** plugin (`ponytail-review`, installed by `setup.sh`); never used unless you ask.

## Starting a thread

| Command | What it does |
|---|---|
| `brigd <name> "task"` | plan with brigd-plan, confirm, run in the TUI |
| `brigd <name> --flow flow.json` | run a hand-written flow |
| `... --bg` | skip the confirm; open the TUI in a new tmux window / Terminal window and return (automatic without a TTY or under Claude Code) |
| `brigd` | open the TUI over all threads |
| `brigd ls` | list threads |
| `brigd status <name>` | per-agent detail |
| `brigd resume <name>` | continue a stopped thread in the TUI |

Run these from inside the git repo the thread should work in.

**Thread names:** start with a lowercase letter, then lowercase letters, digits, `-` or `_`; at most 12 characters;
not one of `ls`, `status`, `resume`, `install-skill`, `help`, `stray`.

**In the TUI**, the `+` button on a repo row (sidebar) opens the new-thread dialog: fill in a name and a task
(Tab switches field, Enter submits, Esc cancels), brigd plans it and asks you to confirm the run.

**Flow basics:** stages run one after another; every agent in a stage runs in parallel, each an interactive Claude
session. The next stage starts when every agent in the current one has written its result file. An agent with a
`worktree` key works in its own git worktree (branch `brigd/<thread>/<key>`); `null` means the main repo.
Two editing agents may not share a worktree.

## TUI keys

Sidebar focused: `j`/`k` or arrows move, `PgUp`/`PgDn` jump, `Enter` open/fold, `h`/`l` or arrows fold,
`Tab` back to the tab, `q` quit. Mouse wheel scrolls; right-click a row for a menu
(agent: Terminate; thread: Delete, Archive).

Anywhere: `Alt-h` / `Alt-l` previous/next tab, `Alt-t` new terminal tab.
(On macOS without "Option as Meta" these are Option-h/l/t.)

**Ctrl-o prefix** (press Ctrl-o, then):

| Key | Action |
|---|---|
| `s` | focus sidebar |
| `n` / `p` | next / previous tab |
| `w` | close tab (agent keeps running) |
| `t` | new terminal tab |
| `v` | split pane |
| `o` | other pane |
| `c` | cycle theme |
| `q` | quit (asks to confirm if agents are running) |
| `Ctrl-o` | send Ctrl-o to the focused claude |

**Ctrl-t thread prefix** (press Ctrl-t, then), acting on the highlighted thread:

| Key | Action |
|---|---|
| `d` | delete the thread with its worktrees and `brigd/` branches (asks `y` to confirm) |
| `a` | archive it: moved to `<root>/archive/threads`, worktrees kept |
| `n` | new thread in the highlighted thread's repo (opens the new-thread dialog) |
| `Ctrl-t` | send Ctrl-t to the focused claude |

## Privacy and security

brigd launches **every** agent with `claude --permission-mode auto`: Claude runs tools (shell commands, file
edits) without asking first. Agents can read and write anything your user can, and anything they read
(files, web pages, dependencies) can try to steer them. Only run flows you trust, in repos you trust; use a
disposable checkout for untrusted code. brigd itself sends nothing anywhere: flow state, prompts and results
live under `~/.brigd/threads/`, and all network traffic comes from the `claude` CLI under its own terms.
See [SECURITY.md](SECURITY.md).

## Uninstall

```sh
cargo uninstall brigd
rm -rf ~/.claude/skills/brigd-plan   # the installed skill
rm -rf ~/.brigd                      # thread state; optional
```

Thread worktrees live under `~/.brigd/worktrees` and add `brigd/<thread>/<key>` branches to your repos;
delete threads from the TUI (`Ctrl-t d`) first to clean those up.

## License

[MIT](LICENSE)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) and the [Code of Conduct](CODE_OF_CONDUCT.md).
Changes are tracked in [CHANGELOG.md](CHANGELOG.md).
