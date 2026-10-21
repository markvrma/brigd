# Changelog

All notable changes to this project are documented here.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- An agent that cannot finish its task asks you a multiple-choice question (shown as blocked) instead of writing its result; a background check marks a "running" thread failed when no claude process is alive.
- New thread in any directory: the header `＋` or `C-t m`; the new-thread dialog has an editable dir field.
- Tabs have a `×` close button and a left-side gradient.

### Fixed

- `brigd install-skill` now also installs `brigd-resume` and `find-skills` (which `plan-review-go` relies on).

## [0.1.0] - unreleased

First public release.

### Added

- Plan a task as a staged flow of Claude Code agents (`brigd <name> "task"`) or run a hand-written one (`--flow flow.json`). Stages run in order; agents within a stage run in parallel, each in its own PTY.
- Per-agent git worktrees (branch `brigd/<thread>/<key>`), with a guard against two editing agents sharing one.
- Single TUI over all threads: sidebar tree of repos, threads and agents, tabs, split panes, terminal tabs, theme cycling, and per-file `+added/-removed` diff counts.
- New-thread dialog (`+` on a repo row, or `Ctrl-t n`), plus `Ctrl-t d` / `Ctrl-t a` to delete or archive a thread.
- `brigd ls`, `brigd status <name>`, `brigd resume <name>`, and `--bg` to open the TUI in a new herdr tab, tmux window or Terminal.app window.
- `brigd install-skill` installs the `brigd-plan` skill for Claude Code.
- Every agent launches with `--permission-mode auto`; see [SECURITY.md](SECURITY.md).

[Unreleased]: https://github.com/markvrma/brigd/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/markvrma/brigd/releases/tag/v0.1.0
