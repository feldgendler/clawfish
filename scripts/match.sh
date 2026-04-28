#!/usr/bin/env bash
# fastchess smoke runner for the chess engine.
#
# Subcommands:
#   self-play    2-game self-play (GreedyMover seed=1 vs seed=2)
#   vs-stockfish 2-game match against Stockfish 18 capped at UCI_Elo=1320
#   compliance   fastchess --compliance UCI shake-out on target/release/clawfish
#
# Usage: scripts/match.sh <subcommand>
#        MATCH_DEBUG=1 scripts/match.sh <subcommand>   (enables set -x)
#
# Adjudication: -maxmoves 300 -resign movecount=3 score=600
# No -draw knob: GreedyMover emits a real score cp, but game trajectories under
# depth-1 greedy tend to be tactical; score-threshold draw filters may fire
# too eagerly. Omitting -draw keeps smoke-game results unambiguous. See ADR-0012.

set -euo pipefail

if [[ "${MATCH_DEBUG:-0}" == "1" ]]; then set -x; fi

# fastchess opens many file descriptors per concurrent game; macOS's default
# ulimit -n (often 256) is too low even at -concurrency 1. Raise to 4096
# (best-effort; do not fail the whole match if the shell forbids the bump).
ulimit -n 4096 2>/dev/null || true

# -------------------------------------------------------------------------
# Pinned fastchess version (must match scripts/install-fastchess.sh).
# -------------------------------------------------------------------------
EXPECTED_VERSION_LINE="alpha 1.8.0"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# -------------------------------------------------------------------------
# Usage.
# -------------------------------------------------------------------------
usage() {
    echo "Usage: scripts/match.sh <subcommand>"
    echo ""
    echo "Subcommands:"
    echo "  self-play      2-game self-play, GreedyMover seed=1 vs seed=2"
    echo "  vs-stockfish   2-game vs Stockfish 18 capped at UCI_Elo=1320"
    echo "  compliance     fastchess --compliance UCI check on target/release/clawfish"
}

if [[ $# -eq 0 ]] || [[ "${1:-}" == "--help" ]] || [[ "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi

SUBCMD="$1"

# -------------------------------------------------------------------------
# fastchess locator.  Prefer the repo-local vendor binary; fall back to PATH.
# Whichever resolves, gate on the version line to catch stale installs.
# -------------------------------------------------------------------------
VENDOR_BINARY="$REPO_ROOT/vendor/fastchess/fastchess"
if [[ -x "$VENDOR_BINARY" ]]; then
    FASTCHESS="$VENDOR_BINARY"
elif command -v fastchess >/dev/null 2>&1; then
    FASTCHESS="$(command -v fastchess)"
else
    echo "ERROR: fastchess not found." >&2
    echo "  Run: scripts/install-fastchess.sh" >&2
    exit 1
fi

actual_ver="$("$FASTCHESS" --version 2>&1 || true)"
if ! echo "$actual_ver" | grep -q "$EXPECTED_VERSION_LINE"; then
    echo "ERROR: found fastchess at $FASTCHESS but it reports:" >&2
    echo "  $actual_ver" >&2
    echo "  expected to contain: $EXPECTED_VERSION_LINE" >&2
    echo "  Run: scripts/install-fastchess.sh" >&2
    exit 1
fi
echo "fastchess resolved: $FASTCHESS"

# -------------------------------------------------------------------------
# Build the engine binary.
# -------------------------------------------------------------------------
echo "Building engine..."
cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml" --quiet
ENGINE="$REPO_ROOT/target/release/clawfish"

# -------------------------------------------------------------------------
# Adjudication flags shared by self-play and vs-stockfish.
# No -draw: see header comment and ADR-0012.
# -------------------------------------------------------------------------
ADJUDICATION=(-maxmoves 300 -resign movecount=3 score=600)

# -------------------------------------------------------------------------
# Output directory.
# -------------------------------------------------------------------------
SMOKE_DIR="$REPO_ROOT/target/matches/smoke"

# -------------------------------------------------------------------------
# Subcommand dispatch.
# -------------------------------------------------------------------------
case "$SUBCMD" in

    compliance)
        echo "# Running: $FASTCHESS --compliance $ENGINE"
        "$FASTCHESS" --compliance "$ENGINE"
        ;;

    self-play)
        mkdir -p "$SMOKE_DIR"
        TS="$(date +%Y%m%dT%H%M%S)"
        PGN="$SMOKE_DIR/m2-self-play-${TS}.pgn"
        LOG="$SMOKE_DIR/m2-self-play-${TS}.log"
        echo "# Output: $PGN"
        echo "# Log:    $LOG"
        "$FASTCHESS" \
            -engine cmd="$ENGINE" name=clawfish-W option.Random_Seed=1 \
            -engine cmd="$ENGINE" name=clawfish-B option.Random_Seed=2 \
            -each proto=uci tc=10+0.1 \
            -rounds 1 -repeat \
            "${ADJUDICATION[@]}" \
            -pgnout file="$PGN" notation=san \
            -log file="$LOG" level=info engine=true
        echo "PGN: $PGN"
        echo "Log: $LOG"
        ;;

    vs-stockfish)
        if ! command -v stockfish >/dev/null 2>&1; then
            echo "ERROR: stockfish not on PATH." >&2
            echo "  Run: brew install stockfish" >&2
            exit 1
        fi
        STOCKFISH="$(command -v stockfish)"
        mkdir -p "$SMOKE_DIR"
        TS="$(date +%Y%m%dT%H%M%S)"
        PGN="$SMOKE_DIR/m2-vs-stockfish-${TS}.pgn"
        LOG="$SMOKE_DIR/m2-vs-stockfish-${TS}.log"
        echo "# Output: $PGN"
        echo "# Log:    $LOG"
        "$FASTCHESS" \
            -engine cmd="$ENGINE" name=clawfish option.Random_Seed=1 \
            -engine cmd="$STOCKFISH" name=sf18 option.UCI_LimitStrength=true option.UCI_Elo=1320 \
            -each proto=uci tc=10+0.1 \
            -rounds 1 -repeat \
            "${ADJUDICATION[@]}" \
            -pgnout file="$PGN" notation=san \
            -log file="$LOG" level=info engine=true
        echo "PGN: $PGN"
        echo "Log: $LOG"
        ;;

    *)
        echo "ERROR: unknown subcommand: $SUBCMD" >&2
        echo "" >&2
        usage >&2
        exit 1
        ;;
esac
