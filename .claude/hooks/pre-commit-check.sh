#!/usr/bin/env bash
# Claude Code PreToolUse hook.
# Intercepts `git commit` Bash invocations and verifies the commit is clean.
#
# Scope: staged-only fmt; conditional clippy/test.
#   - `rustfmt --check` runs against staged .rs files only — parallel agents'
#     in-flight unstaged or untracked work doesn't block your commit.
#   - `cargo clippy` + `cargo test` are whole-crate, but gated on `cargo check`
#     first confirming the workspace compiles. If it doesn't (typically a
#     parallel agent mid-edit), we skip with a note rather than block.
#
# Exits 2 with stderr to block the commit and surface output to the agent.

set -uo pipefail

input=$(cat)

cmd=$(printf '%s' "$input" | python3 -c '
import json, sys
try:
    data = json.loads(sys.stdin.read())
    print(data.get("tool_input", {}).get("command", ""))
except Exception:
    pass
' 2>/dev/null || true)

case "$cmd" in
  *"git commit "*|*"git commit") ;;
  *) exit 0 ;;
esac

cd "${CLAUDE_PROJECT_DIR:-$(pwd)}" || exit 0

fail() {
  echo "pre-commit hook: $1 failed — fix before committing" >&2
  exit 2
}

# Staged .rs files (added/copied/modified/renamed only — not deleted).
staged_rs=$(git diff --cached --name-only --diff-filter=ACMR -- '*.rs' 2>/dev/null || true)

if [ -n "$staged_rs" ]; then
  edition=$(grep -E '^edition[[:space:]]*=' Cargo.toml 2>/dev/null | head -1 | sed -E 's/.*"([0-9]+)".*/\1/')
  edition="${edition:-2024}"
  # shellcheck disable=SC2086
  rustfmt --check --edition "$edition" $staged_rs >&2 || fail "rustfmt --check (staged .rs files)"
fi

# Run whole-crate clippy + tests only if the working tree exactly matches the staged set
# (no unstaged tracked changes, no untracked files). Otherwise, parallel/in-flight work
# may legitimately leave the workspace non-compiling or in TDD-red, and our commit
# (which doesn't touch those scopes) shouldn't be blocked by it.
if git diff --quiet 2>/dev/null \
   && [ -z "$(git ls-files --others --exclude-standard 2>/dev/null)" ] \
   && cargo check --quiet --all-targets >/dev/null 2>&1; then
  cargo clippy --quiet --all-targets -- -D warnings >&2 || fail "cargo clippy"
  cargo test --quiet >&2 || fail "cargo test"
  echo "pre-commit hook: rustfmt (staged) + clippy + tests green" >&2
else
  echo "pre-commit hook: rustfmt (staged) clean; workspace has parallel/in-flight changes outside the staged set, skipping clippy + tests" >&2
fi

exit 0
