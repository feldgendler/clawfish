#!/usr/bin/env bash
# Claude Code PreToolUse hook.
# Intercepts `git commit` Bash invocations and verifies the commit is clean.
#
# Scope:
#   - `cargo fmt --all -- --check` whole-workspace (matches the CI step at
#     .github/workflows/ci.yml exactly). Previously this hook ran a
#     per-staged-file `rustfmt --check`; that scope can mask workspace-level
#     drift (e.g., a re-edit that landed unformatted in `git add -p` then
#     wasn't re-checked). M5.H1's CI fmt failure (2026-05-10) slipped past
#     the per-file form despite producing an identical diff to the workspace
#     form — adopting the CI-matching form closes the gap.
#   - `cd fuzz && cargo fmt --all -- --check` for the fuzz workspace (also
#     matches CI).
#   - `cargo clippy` + `cargo test` are whole-crate, gated on `cargo check`
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

# Whole-workspace rustfmt check — matches CI exactly. We previously ran a
# per-staged-file `rustfmt --check`; both forms produce identical diffs on
# individual files, but the workspace form catches drift across all workspace
# members (including the fuzz workspace below) in a single invocation and is
# what CI runs at .github/workflows/ci.yml. M5.H1 (commit 339fd7e) shipped
# with 3 fmt violations in src/search.rs that CI flagged retroactively;
# unconditional workspace fmt closes the gap.
#
# We DON'T gate this on the "no parallel-agent WIP" check used by clippy/test
# below: rustfmt is deterministic, fast, and never blocks on uncommitted work.
cargo fmt --all -- --check >&2 || fail "cargo fmt --all -- --check"

# Fuzz workspace has its own Cargo.toml outside the root workspace; check it
# separately, matching the second fmt step in CI.
if [ -d fuzz ]; then
  ( cd fuzz && cargo fmt --all -- --check ) >&2 || fail "cargo fmt --all -- --check (fuzz workspace)"
fi

# Run whole-crate clippy + tests only if the working tree exactly matches the staged set
# (no unstaged tracked changes, no untracked files). Otherwise, parallel/in-flight work
# may legitimately leave the workspace non-compiling or in TDD-red, and our commit
# (which doesn't touch those scopes) shouldn't be blocked by it.
if git diff --quiet 2>/dev/null \
   && [ -z "$(git ls-files --others --exclude-standard 2>/dev/null)" ] \
   && cargo check --locked --quiet --all-targets >/dev/null 2>&1; then
  # --locked: use Cargo.lock exactly (matches CI).
  # --all-targets on test: includes integration tests + benches (matches CI;
  #   was missing in the old hook, which would have skipped tests/uci_integration.rs).
  cargo clippy --locked --quiet --all-targets -- -D warnings >&2 || fail "cargo clippy"
  cargo test --locked --quiet --all-targets >&2 || fail "cargo test"
  echo "pre-commit hook: cargo fmt --all (+ fuzz) + clippy + tests green" >&2
else
  echo "pre-commit hook: cargo fmt --all clean; workspace has parallel/in-flight changes outside the staged set, skipping clippy + tests" >&2
fi

exit 0
