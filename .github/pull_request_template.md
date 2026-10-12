## What and why

<!-- One or two sentences. Link the issue: Closes #123 -->

## Checklist

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test` (tests never launch a real `claude` or touch the network)
- [ ] Commit messages follow CONTRIBUTING.md
- [ ] README/key docs updated if behaviour changed
- [ ] Tried it in a real terminal, if this touches the TUI, PTYs or agent launching
