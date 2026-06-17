#!/usr/bin/env bash
# M7.B.1 — qsearch SEE-prune threshold sweep (negative-margin tune).
# Sweeps QS_SEE_PRUNE_THRESHOLD over a list of values, running a full
# mixed-TC + virtual-clock SPRT vs M5.K for each (same shape/seed as the
# M7.B ship run). Restores the const to 0 on exit (always).
#
# Usage: scripts/m7b1-sweep.sh <seed-hex> <threshold> [<threshold> ...]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"
SEED="$1"; shift
THRESHOLDS=("$@")
SRC="src/search.rs"
RESULTS="bench/sprt/m7b1-sweep"
mkdir -p "$RESULTS"

restore() {
    sed -i '' -E 's/^const QS_SEE_PRUNE_THRESHOLD: i32 = .*/const QS_SEE_PRUNE_THRESHOLD: i32 = 0;/' "$SRC"
    echo "[sweep] restored QS_SEE_PRUNE_THRESHOLD = 0"
}
trap restore EXIT

SEED_SLUG="${SEED: -3}"  # last 3 hex chars, e.g. 00B

for T in "${THRESHOLDS[@]}"; do
    TAG="thr${T}-seed${SEED_SLUG}"
    echo "================================================================"
    echo "[sweep] $(date '+%H:%M:%S') START $TAG  (QS_SEE_PRUNE_THRESHOLD=$T, seed=$SEED)"
    echo "================================================================"
    sed -i '' -E "s/^const QS_SEE_PRUNE_THRESHOLD: i32 = .*/const QS_SEE_PRUNE_THRESHOLD: i32 = ${T};/" "$SRC"
    grep -nE '^const QS_SEE_PRUNE_THRESHOLD' "$SRC"

    SPRT_TC_SAMPLE='10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1' \
    SPRT_VIRTUAL_CLOCK=1 \
    SPRT_ELO1=5 \
    SPRT_SEED="$SEED" \
    SPRT_GAMES=400 \
    SPRT_CONCURRENCY=6 \
    scripts/sprt.sh sprt M5.K

    # newest sprt run dir = this run
    RUN_DIR="$(ls -dt target/matches/sprt/*-M5.K-sprt | head -1)"
    cp "$RUN_DIR/summary.txt" "$RESULTS/${TAG}-summary.txt"
    echo "$RUN_DIR" > "$RESULTS/${TAG}-rundir.txt"
    echo "[sweep] $(date '+%H:%M:%S') DONE $TAG -> $RUN_DIR"
    echo "[sweep] key lines:"
    grep -E '^(converged|summary-by-tc|sprt|ci):' "$RESULTS/${TAG}-summary.txt" || true
done

echo "[sweep] $(date '+%H:%M:%S') ALL DONE"
