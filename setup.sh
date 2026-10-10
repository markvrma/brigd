#!/usr/bin/env bash
# One-shot, idempotent brigd setup. Assumes Claude Code is the agent.
set -euo pipefail

REPO_URL="git@github-personal:markvrma/brigd.git"

die() { echo "error: $*" >&2; exit 1; }

command -v git >/dev/null || die "git not found; install git first."
command -v cargo >/dev/null || die "cargo not found. Install Rust with:
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
then re-run ./setup.sh"
command -v claude >/dev/null || die "claude (Claude Code) not found. Install it with:
  npm install -g @anthropic-ai/claude-code
or see https://docs.claude.com/en/docs/claude-code/setup, then log in with 'claude'."

# Run from a brigd checkout, else clone/pull into \$BRIGD_DIR.
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if grep -qs '^name = "brigd"' "$here/Cargo.toml"; then
  cd "$here"
else
  dir="${BRIGD_DIR:-$HOME/brigd}"
  if [ -d "$dir/.git" ]; then
    git -C "$dir" pull --ff-only
  else
    git clone "$REPO_URL" "$dir"
  fi
  cd "$dir"
fi

if [ -f Cargo.lock ]; then
  cargo install --path . --locked
else
  cargo install --path .
fi

cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
case ":$PATH:" in
  *":$cargo_bin:"*) ;;
  *) echo "warning: $cargo_bin is not on your PATH; add it to run brigd." >&2 ;;
esac

"$cargo_bin/brigd" install-skill

if command -v herdr >/dev/null; then echo "found herdr: --bg will use it."
elif command -v tmux >/dev/null; then echo "found tmux: --bg will use it."
else echo "no herdr or tmux: --bg falls back to Terminal.app (macOS only)."
fi

cat <<'MSG'

Done. Next steps:
  brigd demo "say hi"        # plan and run a first thread (from inside a git repo)
  /brigd-plan <task>         # inside Claude Code: returns a flow JSON
MSG
