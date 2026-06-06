#!/usr/bin/env bash
# One-off SPRT runner for the depth-gated adaptive aspiration candidate
# (delta-baseline lever 1). SAME-BINARY self-SPRT: candidate = HEAD (89e4dad)
# with the [8,12]-banded adaptive UCI options; baseline = HEAD with adaptive
# OFF (byte-identical to M5.F.1, bench d7 1354640). Using one binary for both
# sides isolates the setoption effect with zero toolchain confound.
#
# Usage: scripts/sprt-depth-gate.sh <seed-hex> <out-subdir> [concurrency]
set -euo pipefail
SEED="$1"; OUTSUB="$2"; CONC="${3:-4}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/clawfish"
HARNESS="$ROOT/target/release/elo-iterate"
OUT="$ROOT/target/matches/sprt/$OUTSUB"
mkdir -p "$OUT"

"$HARNESS" \
    --engine "$BIN" --opponent "$BIN" \
    --engine-option Aspiration_Adaptive=true \
    --engine-option Aspiration_AdaptiveMinDepth=8 \
    --engine-option Aspiration_AdaptiveMaxDepth=12 \
    --virtual-clock \
    --tc-sample '10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1' \
    --seed "$SEED" \
    --concurrency "$CONC" \
    --resign-movecount 3 --resign-score 600 \
    --draw-movenumber 34 --draw-movecount 8 --draw-score 20 \
    --max-moves 200 \
    --max-games 400 \
    --initial-elo 0 \
    --sprt-elo0 0 --sprt-elo1 10 --sprt-alpha 0.05 --sprt-beta 0.05 \
    --out-dir "$OUT"
