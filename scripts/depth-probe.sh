#!/usr/bin/env bash
# Ad-hoc depth-reach probe (M5.I / M5.H2 precondition check).
# Feeds the 8 middlegame FENs from src/bench.rs to the production binary at a
# real 20+0.2 clock (fresh per position) and reports the deepest completed
# `info depth N`. Holds stdin open with a trailing sleep so the engine's reader
# thread does not trip the stop flag before the search runs. NOT a committed
# tool — scratch.
set -euo pipefail

BIN="${1:-target/release/clawfish}"
WTIME="${2:-20000}"   # 20s base
WINC="${3:-200}"      # 0.2s increment  -> 20+0.2
HOLD="${4:-7}"        # seconds to hold stdin open

FENS=(
  "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1"
  "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2pP/R2Q1RK1 w kq - 0 1"
  "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8"
  "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10"
  "r1bqr1k1/pp1nbppp/2p2n2/3p4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9"
  "r2q1rk1/pp1bbppp/2n1pn2/3p4/3P4/2NBPN2/PP1B1PPP/R2Q1RK1 w - - 6 11"
  "2r3k1/p4ppp/4p3/3pP3/3P4/2P2N2/P4PPP/3R2K1 b - - 0 20"
  "r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQK2R w KQ - 0 8"
)

echo "binary=$BIN  TC=${WTIME}ms+${WINC}ms (fresh clock/pos)  positions=${#FENS[@]}"
depths=()
for fen in "${FENS[@]}"; do
  out=$( { printf 'uci\nisready\nposition fen %s\ngo wtime %s btime %s winc %s binc %s\n' \
            "$fen" "$WTIME" "$WTIME" "$WINC" "$WINC"; sleep "$HOLD"; } \
          | "$BIN" 2>/dev/null )
  d=$(printf '%s\n' "$out" | grep -oE 'info depth [0-9]+' | grep -oE '[0-9]+' | sort -n | tail -1)
  depths+=("${d:-0}")
  printf '  depth %2s  %s\n' "${d:-0}" "$fen"
done

sorted=$(printf '%s\n' "${depths[@]}" | sort -n)
n=${#depths[@]}; mid=$(( n / 2 ))
median=$(printf '%s\n' "$sorted" | sed -n "$((mid))p")  # lower-median of 8
median_hi=$(printf '%s\n' "$sorted" | sed -n "$((mid+1))p")
echo "depths(sorted): $(printf '%s ' $sorted)"
echo "MEDIAN_DEPTH (lower/upper of 8) = ${median}/${median_hi}   precondition = >= 14"
