# Contributing to brigd

Thanks for helping. Small PRs beat big ones; open an issue first for anything large.

## Prerequisites

- Rust 1.89 or newer (via [rustup](https://rustup.rs)); CI pins an exact toolchain, see `.github/workflows/ci.yml`
- `git`
- [Claude Code](https://docs.claude.com/en/docs/claude-code) (`claude`), only to run brigd for real; the tests don't need it
- `tmux` or `herdr` (optional: `--bg`)

## Build, run, test

```sh
cargo build --release
cargo run --release -- <name> --flow flow.json   # run a flow in the TUI
cargo test                                       # offline, never launches claude
```

Agents run with `--permission-mode auto`, so try flows in a throwaway repo. Never make a default test launch `claude` or hit the network.

## Code layout

Each module only imports the ones listed below it, so a file can be read without its callers.

| File | What lives there |
|---|---|
| `src/main.rs` | CLI: argument dispatch, `ls`/`status`/`resume`, new threads, `install-skill`; the "flow" glossary |
| `src/tui/` | ratatui UI. `mod.rs` holds `App`, the run loop and the contracts (draw order, key precedence, locks); `draw`, `input`, `sidebar`, `tabs`, `newt` (new-thread window), `threads` (thread/agent actions), `flowplan` (FLOWPLAN and flow.md rendering), `markdown`, `term`, `theme`, `tests` |
| `src/engine.rs` | Running a flow: stages, supervising each agent until its output.md, reconcile, terminate/stop |
| `src/stray.rs` | The `claude` shim and stray sessions started inside brigd terminals |
| `src/launch.rs` | `--bg`: starting `brigd resume` in herdr, tmux or Terminal, or handing it to the open TUI (`tui.pid`) |
| `src/runner.rs` | One agent = one PTY child; reader thread feeds a vt100 parser; status via claude hooks |
| `src/tree.rs` | Sidebar tree read from disk: repo, thread, agent, outputs, changed files |
| `src/diffs.rs` | Agent diffs: the base commit, `git diff` listings and views, sealing, revert |
| `src/transcript.rs` | Claude transcript files: status, changed files, final text, subagent mirroring |
| `src/flow.rs` | The flow definition: schema, validation, the planner call, prompt.md, claude flags |
| `src/store.rs` | Thread data on disk (`~/.brigd/threads/<name>/`): `Flow`, `State`, `Status`, thread names, create/archive/delete |
| `skills/brigd-plan/` | The `brigd-plan` Claude Code skill, embedded into the binary and installed by `brigd install-skill` |

## Commit style

Conventional commits: `type(scope): subject`, lowercase, imperative, no trailing period. Types in use: `feat`, `fix`, `docs`, `chore`, `ci`.

```
feat(tui): Ctrl-t thread prefix (d delete, a archive, n new thread)
fix(tui): disarm prefixes on Ctrl-o and when a modal or mouse click intervenes
docs: README and setup.sh
```

## Pull request checklist

- [ ] `cargo fmt --all` (CI runs `cargo fmt --all --check`)
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test` passes
- [ ] New logic has a test, and tests are offline
- [ ] Commit messages follow the style above
- [ ] README/keys docs updated if user-visible behaviour changed

## Conduct

See [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
