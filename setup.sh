#!/usr/bin/env bash
# One-shot, idempotent brigd setup. Needs any one of claude, codex, pi, kimi.
set -euo pipefail

REPO_URL="https://github.com/markvrma/brigd.git"

die() { echo "error: $*" >&2; exit 1; }

command -v git >/dev/null || die "git not found; install git first."
command -v cargo >/dev/null || die "cargo not found. Install Rust with:
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
then re-run ./setup.sh"
have_agent=
for a in claude codex pi kimi; do command -v "$a" >/dev/null && have_agent=1; done
[ -n "$have_agent" ] || die "no coding agent found; install one of: claude, codex, pi, kimi.
For claude: npm install -g @anthropic-ai/claude-code, then log in with 'claude'."

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

"$cargo_bin/brigd" install-skill   # brigd-plan, plan-review-go, grill-me, grilling, brigd-resume, find-skills

# ponytail plugin (ponytail-review) for the opt-in plan-review-go path; non-fatal.
if command -v claude >/dev/null; then
claude plugin marketplace add DietrichGebert/ponytail >/dev/null 2>&1 || true # already added is fine
claude plugin install ponytail@ponytail || echo "warning: ponytail install failed; run: claude plugin install ponytail@ponytail" >&2
fi

if command -v herdr >/dev/null; then echo "found herdr: --bg will use it."
elif command -v tmux >/dev/null; then echo "found tmux: --bg will use it."
else echo "no herdr or tmux: --bg falls back to Terminal.app (macOS only)."
fi

cat <<'MSG'

Done. Next steps:
  brigd demo "say hi"        # plan and run a first thread (from inside a git repo)
  /brigd-plan <task>         # inside Claude Code: returns a flow JSON
MSG
