#!/usr/bin/env bash
# Smoke runner for the chess engine.
#
# Subcommands:
#   self-play    2-game self-play, both sides built from HEAD with distinct Random_Seed
#                via the in-process harness (`elo-iterate`).
#   vs-stockfish 2-game match against Stockfish 18 capped at UCI_Elo=1320,
#                via the in-process harness.
#   compliance   fastchess --compliance UCI shake-out on target/release/clawfish.
#                This is the only subcommand that still uses fastchess; all other
#                flows have moved to the in-process harness as of ELOH.E.
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

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# -------------------------------------------------------------------------
# Usage.
# -------------------------------------------------------------------------
usage() {
    echo "Usage: scripts/match.sh <subcommand>"
    echo ""
    echo "Subcommands:"
    echo "  self-play      2-game self-play (in-process harness; Random_Seed=1 vs Random_Seed=2)"
    echo "  vs-stockfish   2-game vs Stockfish 18 capped at UCI_Elo=1320 (in-process harness)"
    echo "  compliance     fastchess --compliance UCI check on target/release/clawfish"
}

if [[ $# -eq 0 ]] || [[ "${1:-}" == "--help" ]] || [[ "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi

SUBCMD="$1"

# -------------------------------------------------------------------------
# Build the engine binary.
# -------------------------------------------------------------------------
echo "Building engine..."
cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml" --quiet
ENGINE="$REPO_ROOT/target/release/clawfish"

# -------------------------------------------------------------------------
# Output directory.
# -------------------------------------------------------------------------
SMOKE_DIR="$REPO_ROOT/target/matches/smoke"

# -------------------------------------------------------------------------
# Subcommand dispatch.
# -------------------------------------------------------------------------
case "$SUBCMD" in

    compliance)
        # fastchess locator (only used by this arm — all other flows are harness-side).
        EXPECTED_VERSION_LINE="alpha 1.8.0"
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
        echo "# Running: $FASTCHESS --compliance $ENGINE"
        "$FASTCHESS" --compliance "$ENGINE"
        ;;

    self-play)
        mkdir -p "$SMOKE_DIR"
        TS="$(date +%Y%m%dT%H%M%S)"
        OUT_DIR="$SMOKE_DIR/m2-self-play-${TS}"
        echo "# Output dir: $OUT_DIR"
        cargo run --release --bin elo-iterate --manifest-path "$REPO_ROOT/Cargo.toml" --quiet -- \
            --engine "$ENGINE" \
            --opponent "$ENGINE" \
            --engine-option "Random_Seed=1" \
            --opponent-option "Random_Seed=2" \
            --tc 10+0.1 \
            --max-games 2 \
            --initial-elo 0 \
            --k0 0 --target-sigma 0 \
            --resign-movecount 3 --resign-score 600 \
            --max-moves 300 \
            --out-dir "$OUT_DIR"
        echo "Output dir: $OUT_DIR"
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
        OUT_DIR="$SMOKE_DIR/m2-vs-stockfish-${TS}"
        echo "# Output dir: $OUT_DIR"
        cargo run --release --bin elo-iterate --manifest-path "$REPO_ROOT/Cargo.toml" --quiet -- \
            --engine "$ENGINE" \
            --opponent "$STOCKFISH" \
            --engine-option "Random_Seed=1" \
            --opponent-option UCI_LimitStrength=true \
            --opponent-option UCI_Elo=1320 \
            --tc 10+0.1 \
            --max-games 2 \
            --initial-elo 0 \
            --k0 0 --target-sigma 0 \
            --resign-movecount 3 --resign-score 600 \
            --max-moves 300 \
            --out-dir "$OUT_DIR"
        echo "Output dir: $OUT_DIR"
        ;;

    *)
        echo "ERROR: unknown subcommand: $SUBCMD" >&2
        echo "" >&2
        usage >&2
        exit 1
        ;;
esac
