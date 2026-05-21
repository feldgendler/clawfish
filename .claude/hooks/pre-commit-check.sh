#!/usr/bin/env bash
# Claude Code PreToolUse hook.
# Intercepts `git commit` Bash invocations and verifies the commit is clean.
#
# Scope (intentionally fast — the contract is "seconds, not minutes"):
#   - `cargo fmt --all -- --check` whole-workspace (matches the CI step at
#     .github/workflows/ci.yml exactly). Previously this hook ran a
#     per-staged-file `rustfmt --check`; that scope can mask workspace-level
#     drift (e.g., a re-edit that landed unformatted in `git add -p` then
#     wasn't re-checked). M5.H1's CI fmt failure (2026-05-10) slipped past
#     the per-file form despite producing an identical diff to the workspace
#     form — adopting the CI-matching form closes the gap.
#   - `cd fuzz && cargo fmt --all -- --check` for the fuzz workspace (also
#     matches CI).
#
# Out of scope (run by CI + explicitly by the developer, not on every commit):
#   - `cargo check`, `cargo clippy`, `cargo test` — these scale with the
#     project (the test suite includes `tests/corpus_reproducibility.rs` and
#     `tests/corpus_crash_safety.rs`, each ~60s; with `--all-targets` also
#     compiling every bench + bin, total wallclock easily exceeds 5 min on
#     warm cache, more on cold). CI runs these on every push; running them
#     in the commit hook is duplicative and turns routine commits into
#     multi-minute waits.
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
cargo fmt --all -- --check >&2 || fail "cargo fmt --all -- --check"

# Fuzz workspace has its own Cargo.toml outside the root workspace; check it
# separately, matching the second fmt step in CI.
if [ -d fuzz ]; then
  ( cd fuzz && cargo fmt --all -- --check ) >&2 || fail "cargo fmt --all -- --check (fuzz workspace)"
fi

echo "pre-commit hook: cargo fmt --all (+ fuzz) clean" >&2
exit 0
