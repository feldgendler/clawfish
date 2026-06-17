#!/usr/bin/env bash
# M7.B.2 — depth-conditioned qsearch SEE-prune ramp: 2-seed validating SPRT.
# HEAD source already carries the ramp (no const/source manipulation here),
# so each seed just builds HEAD and plays vs M5.K. Same mixed-TC + virtual-clock
# shape and seeds as M7.B / M7.B.1, so per-TC is directly comparable.
#
# Launched as a harness-tracked run_in_background task (NOT nohup) per
# docs/workflow.md "Background-process launch discipline".
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"
RESULTS="bench/sprt/m7b1-sweep"
mkdir -p "$RESULTS"

for SEED in 0xC1ABF15AE10DD00B 0xC1ABF15AE10DD01B; do
    SLUG="${SEED: -3}"
    echo "================================================================"
    echo "[m7b2] $(date '+%H:%M:%S') START ramp SPRT seed${SLUG} (seed=$SEED)"
    echo "================================================================"
    SPRT_TC_SAMPLE='10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1' \
    SPRT_VIRTUAL_CLOCK=1 \
    SPRT_ELO1=5 \
    SPRT_SEED="$SEED" \
    SPRT_GAMES=400 \
    SPRT_CONCURRENCY=6 \
    scripts/sprt.sh sprt M5.K

    RUN_DIR="$(ls -dt target/matches/sprt/*-M5.K-sprt | head -1)"
    cp "$RUN_DIR/summary.txt" "$RESULTS/m7b2-ramp-seed${SLUG}-summary.txt"
    echo "$RUN_DIR" > "$RESULTS/m7b2-ramp-seed${SLUG}-rundir.txt"
    echo "[m7b2] $(date '+%H:%M:%S') DONE seed${SLUG} -> $RUN_DIR"
    grep -E '^(converged|summary-by-tc|sprt|ci):' "$RESULTS/m7b2-ramp-seed${SLUG}-summary.txt" || true
done
echo "[m7b2] $(date '+%H:%M:%S') ALL DONE"
