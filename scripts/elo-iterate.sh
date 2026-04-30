#!/usr/bin/env bash
# Online Elo iteration via the in-process Rust harness (ELOH.B).
#
# This is a thin wrapper over `cargo run --release --bin elo-iterate`. The
# bash CLI surface (`<initial> [batches] [games-per-batch]`) is preserved
# for compatibility with prior invocations, but the K-update cadence shifts
# from per-batch (the historical fastchess-driven version) to per-game
# (the new in-process harness, per ELOH.B spec). Total game count is
# preserved (`BATCHES * GAMES_PER_BATCH = --max-games`).
#
# Usage:
#   scripts/elo-iterate.sh <initial-estimate> [batches] [games-per-batch]
# Defaults: initial=2171, batches=30, games-per-batch=4.
#
# Environment overrides:
#   SPRT_TC          (default 10+0.1)
#   SPRT_CONCURRENCY (default = games-per-batch / 2; color-pairs, not games)

set -euo pipefail

if [[ "${SPRT_DEBUG:-0}" == "1" ]]; then set -x; fi
ulimit -n 4096 2>/dev/null || true

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INITIAL="${1:-2171}"
BATCHES="${2:-30}"
GAMES_PER_BATCH="${3:-4}"
if (( GAMES_PER_BATCH % 2 != 0 )); then
    echo "ERROR: games-per-batch must be even (color-pair); got $GAMES_PER_BATCH" >&2
    exit 1
fi

TC="${SPRT_TC:-10+0.1}"
CONCURRENCY="${SPRT_CONCURRENCY:-$((GAMES_PER_BATCH / 2))}"
TOTAL_GAMES=$((BATCHES * GAMES_PER_BATCH))

echo "Building HEAD binary..."
cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml" --quiet
CURRENT_BINARY="$REPO_ROOT/target/release/clawfish"

if ! command -v stockfish >/dev/null 2>&1; then
    echo "ERROR: stockfish not on PATH; brew install stockfish" >&2
    exit 1
fi
STOCKFISH="$(command -v stockfish)"

TS="$(date +%Y%m%dT%H%M%S)"
OUT_DIR="$REPO_ROOT/target/elo-iterate/run-$TS"

cargo run --release --bin elo-iterate --manifest-path "$REPO_ROOT/Cargo.toml" --quiet -- \
    --engine "$CURRENT_BINARY" \
    --opponent "$STOCKFISH" \
    --engine-launch-prefix "taskpolicy -c utility" \
    --opponent-launch-prefix "taskpolicy -c utility" \
    --opponent-option UCI_LimitStrength=true \
    --tc "$TC" \
    --max-games "$TOTAL_GAMES" \
    --concurrency "$CONCURRENCY" \
    --initial-elo "$INITIAL" \
    --k0 40 --tau 10 \
    --target-sigma 30 --stop-window 30 --stop-window-confirm 5 \
    --out-dir "$OUT_DIR"

echo "Output dir: $OUT_DIR"
