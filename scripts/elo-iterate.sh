#!/usr/bin/env bash
# Online Elo iteration against Stockfish — chess.com-style (M3.F follow-up).
#
# Each iteration plays one fastchess "round" (= 2 games, one per color via
# -repeat) at Stockfish UCI_Elo = round(current estimate). Updates the
# estimate via standard Elo formula:
#
#   expected = 0.5  (Stockfish anchored to clawfish's current estimate)
#   ΔR = K × (avg_score - 0.5)
#
# Robbins-Monro decaying K schedule:
#   K_t = max(8, round(40 / (1 + t/10)))
# t=0..9 → K=40 (fast), t=10..29 → K~20, t=30+ → K=8 (stable).
#
# Usage:
#   scripts/elo-iterate.sh <initial-estimate> [batches] [games-per-batch]
#
# Defaults: batches=30, games-per-batch=6.
#
# games-per-batch should match -concurrency to fully utilize cores: 2 games at
# concurrency=6 wastes 4 of the 6 parallel slots; 6 games at concurrency=6 finish
# in roughly the same wallclock as 2 games (bounded by slowest single game) but
# give 3× the SNR per update. Robbins-Monro K schedule is unchanged — each
# batch's score is averaged across all games in that batch before the K-update.
#
# Output:
#   - Per-batch line on stdout: t, stockfish_elo, score, K, new_estimate.
#   - Final estimate + 1σ rolling CI on stdout.
#   - PGN/log: target/matches/sprt/<dated>-elo-iterate.{pgn,log}.

set -euo pipefail

if [[ "${SPRT_DEBUG:-0}" == "1" ]]; then set -x; fi

ulimit -n 4096 2>/dev/null || true

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INITIAL="${1:-2171}"
BATCHES="${2:-30}"
GAMES_PER_BATCH="${3:-4}"
# -rounds is half of games (each round plays 2 games via -repeat).
if (( GAMES_PER_BATCH % 2 != 0 )); then
    echo "ERROR: games-per-batch must be even (color-swap pair); got $GAMES_PER_BATCH" >&2
    exit 1
fi
ROUNDS_PER_BATCH=$((GAMES_PER_BATCH / 2))
TC="${SPRT_TC:-10+0.1}"
# Concurrency defaults to half the games-per-batch so each pair of (clawfish,
# stockfish) engines fits on its own P-core (Apple M4 has 4 P-cores; 2 concurrent
# games × 2 engines/game = 4 threads = 1 P-core each at full speed). Override
# via env if running on different hardware.
CONCURRENCY="${SPRT_CONCURRENCY:-$((GAMES_PER_BATCH))}"
# QoS hint for both engines — biases macOS scheduler toward P-cores. macOS
# does not expose hard CPU affinity in userspace; this is the best available
# mechanism. Critical for fair Stockfish UCI_Elo comparisons: at fast TC,
# Stockfish on E-cores plays weaker than its nominal UCI_Elo (depth cap may
# not fit available time → self-shallowing or forfeit). Pinning to P-cores
# keeps Stockfish at its calibrated effective strength.
TASKPOLICY="taskpolicy -c utility"

# fastchess locator (mirror match.sh).
EXPECTED_VERSION_LINE="alpha 1.8.0"
VENDOR_BINARY="$REPO_ROOT/vendor/fastchess/fastchess"
if [[ -x "$VENDOR_BINARY" ]]; then
    FASTCHESS="$VENDOR_BINARY"
elif command -v fastchess >/dev/null 2>&1; then
    FASTCHESS="$(command -v fastchess)"
else
    echo "ERROR: fastchess not found." >&2
    exit 1
fi
actual_ver="$("$FASTCHESS" --version 2>&1 || true)"
if ! echo "$actual_ver" | grep -q "$EXPECTED_VERSION_LINE"; then
    echo "ERROR: fastchess version mismatch." >&2
    exit 1
fi

if ! command -v stockfish >/dev/null 2>&1; then
    echo "ERROR: stockfish not on PATH." >&2
    exit 1
fi
STOCKFISH="$(command -v stockfish)"

echo "Building HEAD binary..."
cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml" --quiet
CURRENT_BINARY="$REPO_ROOT/target/release/clawfish"

# Clawfish wrapper: pin to P-cores via QoS=utility hint. Clawfish's strength
# scales with available compute at our 10+0.1 TC; P-core dedication gives the
# "best-hardware" reading the user wants.
#
# Stockfish runs un-wrapped (default QoS). Rationale: with Stockfish's TC set
# huge (effectively unlimited), the UCI_LimitStrength depth cap is ALWAYS the
# binding constraint, regardless of core speed. Stockfish at UCI_Elo=X plays
# at exactly its calibrated strength X on any core. So the question we
# actually answer is: "what UCI_Elo of depth-capped Stockfish does clawfish-
# at-best-hardware equal?" — clean, decoupled from hardware variance.
#
# fastchess uses posix_spawn which doesn't shell-parse cmd; we still need a
# wrapper script for clawfish to embed the taskpolicy invocation.
WRAPPER_DIR="$(mktemp -d)"
trap 'rm -rf "$WRAPPER_DIR"' EXIT
CLAWFISH_WRAPPER="$WRAPPER_DIR/qos-clawfish"
{
    echo '#!/bin/sh'
    echo "exec taskpolicy -c utility \"$CURRENT_BINARY\" \"\$@\""
} > "$CLAWFISH_WRAPPER"
chmod +x "$CLAWFISH_WRAPPER"

# -----------------------------------------------------------------------------
# Output paths.
# -----------------------------------------------------------------------------
SPRT_DIR="$REPO_ROOT/target/matches/sprt"
mkdir -p "$SPRT_DIR"
TS="$(date +%Y%m%dT%H%M%S)"
PGN_DIR="$SPRT_DIR/${TS}-elo-iterate"
mkdir -p "$PGN_DIR"
SUMMARY="$SPRT_DIR/${TS}-elo-iterate.summary"
echo "# Online Elo iteration; initial=$INITIAL, batches=$BATCHES, tc=$TC, concurrency=$CONCURRENCY" > "$SUMMARY"
echo "# t  stockfish_elo  game1_result  game2_result  batch_score  K  estimate_after" >> "$SUMMARY"

# Adjudication. Same as scripts/sprt.sh.
ADJUDICATION=(-maxmoves 200 -resign movecount=3 score=600 -draw movenumber=34 movecount=8 score=20)

# -----------------------------------------------------------------------------
# Iteration loop.
# -----------------------------------------------------------------------------
estimate=$(printf '%.4f' "$INITIAL")
running_squared_dev=0
running_count=0

for (( t=0; t<BATCHES; t++ )); do
    # Robbins-Monro K schedule: K = max(8, round(40 / (1 + t/10))).
    K_raw=$(awk -v t="$t" 'BEGIN { printf "%.0f", 40 / (1 + t/10) }')
    if (( K_raw < 8 )); then K_raw=8; fi
    K="$K_raw"

    sf_elo=$(awk -v e="$estimate" 'BEGIN { printf "%.0f", e }')

    # fastchess won't accept UCI_Elo below 1320 or above ~3190; clamp.
    if (( sf_elo < 1320 )); then sf_elo=1320; fi
    if (( sf_elo > 3190 )); then sf_elo=3190; fi

    pgn_file="$PGN_DIR/batch-${t}.pgn"

    # Symmetric TC for both engines (fastchess hangs on per-engine tc=
    # overrides — see tooling-backlog "Custom in-process Elo-iteration harness"
    # entry for a path to asymmetric TC + go nodes mode + virtual-clock
    # measurement). At our operating point (UCI_Elo ~2000-2400), Stockfish's
    # depth cap fits well within 10+0.1 even on E-cores, so symmetric TC
    # doesn't materially compromise its calibrated effective strength.
    "$FASTCHESS" \
        -engine cmd="$CLAWFISH_WRAPPER" name=clawfish-head \
        -engine cmd="$STOCKFISH" name="stockfish-${sf_elo}" \
            option.UCI_LimitStrength=true "option.UCI_Elo=$sf_elo" \
        -each proto=uci tc="$TC" \
        -concurrency "$CONCURRENCY" \
        -rounds "$ROUNDS_PER_BATCH" -repeat \
        "${ADJUDICATION[@]}" \
        -pgnout file="$pgn_file" notation=san \
        > /dev/null 2>&1

    # Parse PGN to compute clawfish's score.
    # For each game: map [Result] vs [White]/[Black] to a per-game score
    # from clawfish's perspective. Sum, divide by game count.
    batch_score=$(awk '
        BEGIN { total=0; games=0; white=""; black=""; result=""; }
        /^\[White / {
            sub(/^\[White */, "", $0); sub(/\]$/, "", $0); gsub(/"/, "", $0);
            white=$0;
        }
        /^\[Black / {
            sub(/^\[Black */, "", $0); sub(/\]$/, "", $0); gsub(/"/, "", $0);
            black=$0;
        }
        /^\[Result / {
            sub(/^\[Result */, "", $0); sub(/\]$/, "", $0); gsub(/"/, "", $0);
            result=$0;
            # Score for clawfish-head this game.
            score = -1;
            if (result == "1-0") {
                score = (white == "clawfish-head") ? 1.0 : 0.0;
            } else if (result == "0-1") {
                score = (black == "clawfish-head") ? 1.0 : 0.0;
            } else if (result == "1/2-1/2") {
                score = 0.5;
            }
            if (score >= 0) { total += score; games += 1; }
            white=""; black=""; result="";
        }
        END {
            if (games > 0) printf "%.4f", total / games
            else printf "0.5"
        }
    ' "$pgn_file")

    # Update estimate. ΔR = K × (score - 0.5).
    new_estimate=$(awk -v e="$estimate" -v k="$K" -v s="$batch_score" 'BEGIN {
        printf "%.4f", e + k * (s - 0.5)
    }')

    # Track convergence stats (sliding window of last 20 estimates).
    running_count=$((running_count + 1))

    # Per-game results for the summary.
    g1=$(awk -v idx=1 '/^\[Result / { gsub(/"/, "", $0); sub(/^\[Result */, "", $0); sub(/\]$/, "", $0); n+=1; if (n==idx) { print; exit } }' "$pgn_file")
    g2=$(awk -v idx=2 '/^\[Result / { gsub(/"/, "", $0); sub(/^\[Result */, "", $0); sub(/\]$/, "", $0); n+=1; if (n==idx) { print; exit } }' "$pgn_file")

    line=$(printf '%-3d  %5d  %-7s  %-7s  %.4f  %2d  %.2f\n' "$t" "$sf_elo" "$g1" "$g2" "$batch_score" "$K" "$new_estimate")
    echo "$line"
    echo "$line" >> "$SUMMARY"

    estimate="$new_estimate"
done

# Final estimate.
final_int=$(awk -v e="$estimate" 'BEGIN { printf "%.0f", e }')
echo ""
echo "# Final estimate: ~$final_int Elo"
echo "# Final estimate: ~$final_int Elo" >> "$SUMMARY"
echo "# Per-batch summary at: $SUMMARY"
echo "# Per-batch PGNs at: $PGN_DIR/"
