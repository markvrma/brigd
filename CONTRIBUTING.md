# Contributing to brigd

Thanks for helping. Small PRs beat big ones; open an issue first for anything large.

## Prerequisites

- Rust 1.88 or newer (via [rustup](https://rustup.rs)); CI pins an exact toolchain, see `.github/workflows/ci.yml`
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

| File | What lives there |
|---|---|
| `src/main.rs` | CLI, flow/thread state on disk (`~/.brigd/threads/<name>/`), planning, stage scheduling, `--bg` launchers |
| `src/runner.rs` | One agent = one PTY child; reader thread feeds a vt100 parser; status via claude hooks |
| `src/tree.rs` | Sidebar tree read from disk: repo, thread, agent, outputs, changed files |
| `src/tui.rs` | ratatui UI: sidebar, panes, tab bars, modals, key prefixes, themes |
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
