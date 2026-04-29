#!/usr/bin/env bash
# SPRT match runner using historical-commit baselines per docs/workflow.md.
#
# Subcommands:
#   sprt <baseline-tag>      — SPRT match: HEAD vs baseline-tag (clawfish-vs-clawfish)
#   match <baseline-tag>     — fixed-game-count match HEAD vs baseline-tag; 200 games default
#   rating-estimate          — fixed-game-count match HEAD vs Stockfish UCI_Elo=1320; 200 games
#                              default. No tag arg — Stockfish from PATH (brew install stockfish)
#                              capped at the known-Elo setting (ADR-0012 reference point).
#                              CAVEAT: Stockfish UCI_Elo is calibrated by Stockfish at slow TC
#                              (typically 60+0.6 or 120+1.2). Running rating-estimate at the
#                              default `tc=10+0.1` will produce a TC-specific point estimate,
#                              not a direct CCRL-equivalent rating. SPRT_TC override available
#                              if you want to bring the TC closer to Stockfish's calibration
#                              reference at the cost of wallclock.
#
# Usage: scripts/sprt.sh sprt baseline/random-mover
#        scripts/sprt.sh match baseline/random-mover
#        scripts/sprt.sh rating-estimate
#        SPRT_GAMES=400 scripts/sprt.sh sprt baseline/random-mover  (override)
#        SPRT_TC=10+0.1 scripts/sprt.sh sprt baseline/random-mover  (override)
#        SPRT_CONCURRENCY=6 scripts/sprt.sh sprt baseline/random-mover
#        SPRT_REBUILD=1 scripts/sprt.sh sprt baseline/random-mover  (force rebuild
#                                                                    of cached baseline)

set -euo pipefail

if [[ "${SPRT_DEBUG:-0}" == "1" ]]; then set -x; fi

# fastchess opens many file descriptors per concurrent game; raise from the
# macOS default (often 256) which is too low even at -concurrency 1.
ulimit -n 4096 2>/dev/null || true

# -----------------------------------------------------------------------------
# Pinned fastchess version (must match scripts/match.sh + scripts/install-fastchess.sh).
# -----------------------------------------------------------------------------
EXPECTED_VERSION_LINE="alpha 1.8.0"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SUBCMD="${1:-}"
BASELINE_TAG="${2:-}"

usage() {
    echo "Usage: scripts/sprt.sh <sprt|match|rating-estimate> [baseline-tag]"
    echo ""
    echo "  sprt <tag>           SPRT match (elo0=0, elo1=10, alpha=0.05, beta=0.05; up to 400 games)"
    echo "  match <tag>          fixed-game-count match (200 games default)"
    echo "  rating-estimate      fixed-game-count match HEAD vs Stockfish UCI_Elo=1320 (200 games)"
    echo ""
    echo "Env vars:"
    echo "  SPRT_GAMES           default sprt=400, match=200, rating-estimate=200"
    echo "  SPRT_TC              default '10+0.1'"
    echo "  SPRT_CONCURRENCY     default '6'"
    echo "  SPRT_REBUILD=1       force rebuild of cached baseline worktree"
    echo "  SPRT_DEBUG=1         enable set -x"
    echo ""
    echo "Output: target/matches/sprt/<dated>-<baseline-slug>-<subcmd>.{pgn,log}"
}

if [[ -z "$SUBCMD" ]] || [[ "$SUBCMD" == "--help" ]] || [[ "$SUBCMD" == "-h" ]]; then
    usage
    exit 0
fi
# `rating-estimate` takes no tag arg; sprt/match require one.
if [[ "$SUBCMD" != "rating-estimate" ]] && [[ -z "$BASELINE_TAG" ]]; then
    usage
    exit 1
fi

# -----------------------------------------------------------------------------
# fastchess locator (mirror match.sh).
# -----------------------------------------------------------------------------
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
    echo "ERROR: fastchess version mismatch. Expected to contain '$EXPECTED_VERSION_LINE'; got: $actual_ver" >&2
    exit 1
fi
echo "fastchess resolved: $FASTCHESS"

# -----------------------------------------------------------------------------
# Build current-tree binary (incremental — fast on no-op rebuild).
# -----------------------------------------------------------------------------
echo "Building HEAD binary..."
cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml" --quiet
CURRENT_BINARY="$REPO_ROOT/target/release/clawfish"

# -----------------------------------------------------------------------------
# Resolve baseline (clawfish-tag-from-worktree, or stockfish-1320 sentinel).
# -----------------------------------------------------------------------------
if [[ "$SUBCMD" == "rating-estimate" ]]; then
    if ! command -v stockfish >/dev/null 2>&1; then
        echo "ERROR: stockfish not on PATH." >&2
        echo "  Run: brew install stockfish" >&2
        exit 1
    fi
    STOCKFISH="$(command -v stockfish)"
    BASELINE_BINARY="$STOCKFISH"
    BASELINE_LABEL="stockfish-1320"
    BASELINE_SLUG="$BASELINE_LABEL"
else
    # Verify baseline tag exists.
    if ! git -C "$REPO_ROOT" rev-parse --verify --quiet "refs/tags/$BASELINE_TAG" >/dev/null; then
        echo "ERROR: baseline tag '$BASELINE_TAG' not found." >&2
        echo "  Available tags: $(git -C "$REPO_ROOT" tag -l 'baseline/*' | tr '\n' ' ')" >&2
        exit 1
    fi

    # Sanitize tag name → filesystem-safe slug.
    BASELINE_SLUG="${BASELINE_TAG//\//-}"
    BASELINE_LABEL="clawfish-${BASELINE_SLUG}"

    # Build baseline binary in worktree (cached after first run, unless SPRT_REBUILD=1).
    BASELINE_DIR="$REPO_ROOT/target/sprt-baselines/$BASELINE_SLUG"
    if [[ "${SPRT_REBUILD:-0}" == "1" ]] && [[ -d "$BASELINE_DIR" ]]; then
        echo "SPRT_REBUILD=1: removing cached worktree at $BASELINE_DIR..."
        git -C "$REPO_ROOT" worktree remove --force "$BASELINE_DIR" 2>/dev/null || rm -rf "$BASELINE_DIR"
    fi
    if [[ ! -d "$BASELINE_DIR" ]]; then
        echo "Setting up baseline worktree at $BASELINE_DIR..."
        mkdir -p "$REPO_ROOT/target/sprt-baselines"
        git -C "$REPO_ROOT" worktree add "$BASELINE_DIR" "$BASELINE_TAG"
    fi
    # Resolve the baseline's binary name from its Cargo.toml. The project was
    # renamed `chess` → `clawfish` between M2.E and M3.A; older baselines
    # produce `target/release/chess`, newer ones produce `target/release/clawfish`.
    # Reading the package name keeps the script forward-compatible with future
    # renames or additions.
    BASELINE_PKG_NAME="$(grep -E '^name *= *"' "$BASELINE_DIR/Cargo.toml" | head -1 | sed -E 's/.*"([^"]+)".*/\1/')"
    BASELINE_BINARY="$BASELINE_DIR/target/release/$BASELINE_PKG_NAME"
    if [[ ! -x "$BASELINE_BINARY" ]]; then
        echo "Building baseline binary at $BASELINE_BINARY..."
        cargo build --release --manifest-path "$BASELINE_DIR/Cargo.toml" --quiet
    else
        echo "Baseline binary cached at $BASELINE_BINARY"
    fi

    # Sanity-check the baseline binary speaks UCI before fastchess starts.
    # Catches "binary built with stale toolchain / corrupt cache" failures up
    # front, mirroring match.sh's fastchess version-line gate.
    if ! printf 'uci\nquit\n' | "$BASELINE_BINARY" 2>/dev/null | grep -q '^uciok$'; then
        echo "ERROR: baseline binary at $BASELINE_BINARY did not emit 'uciok' on a basic uci probe." >&2
        echo "  Try: SPRT_REBUILD=1 scripts/sprt.sh $SUBCMD $BASELINE_TAG" >&2
        exit 1
    fi
fi

# -----------------------------------------------------------------------------
# Output paths.
# -----------------------------------------------------------------------------
SPRT_DIR="$REPO_ROOT/target/matches/sprt"
mkdir -p "$SPRT_DIR"
TS="$(date +%Y%m%dT%H%M%S)"
PGN="$SPRT_DIR/${TS}-${BASELINE_SLUG}-${SUBCMD}.pgn"
LOG="$SPRT_DIR/${TS}-${BASELINE_SLUG}-${SUBCMD}.log"

# -----------------------------------------------------------------------------
# Match parameters.
# -----------------------------------------------------------------------------
TC="${SPRT_TC:-10+0.1}"
CONCURRENCY="${SPRT_CONCURRENCY:-6}"
ADJUDICATION=(-maxmoves 200 -resign movecount=3 score=600 -draw movenumber=34 movecount=8 score=20)

# Per-engine line for the BASELINE side. For clawfish baselines: just `cmd name`.
# For Stockfish rating-estimate: append the UCI_LimitStrength + UCI_Elo options.
if [[ "$SUBCMD" == "rating-estimate" ]]; then
    BASELINE_ENGINE_ARGS=(
        -engine cmd="$BASELINE_BINARY" name="$BASELINE_LABEL"
        option.UCI_LimitStrength=true option.UCI_Elo=1320
    )
else
    BASELINE_ENGINE_ARGS=(-engine cmd="$BASELINE_BINARY" name="$BASELINE_LABEL")
fi

COMMON=(
    -engine cmd="$CURRENT_BINARY" name=clawfish-head
    "${BASELINE_ENGINE_ARGS[@]}"
    -each proto=uci tc="$TC"
    -concurrency "$CONCURRENCY"
    -repeat
    "${ADJUDICATION[@]}"
    -pgnout file="$PGN" notation=san
    -log file="$LOG" level=info engine=true
    -report penta=true
)

# -----------------------------------------------------------------------------
# Subcommand dispatch.
# -----------------------------------------------------------------------------
case "$SUBCMD" in
    sprt)
        GAMES="${SPRT_GAMES:-400}"
        ROUNDS=$((GAMES / 2))
        echo "Running SPRT: tc=$TC, up to $GAMES games, elo0=0 elo1=10 alpha=0.05 beta=0.05"
        echo "  HEAD vs $BASELINE_LABEL"
        "$FASTCHESS" \
            "${COMMON[@]}" \
            -rounds "$ROUNDS" \
            -sprt elo0=0 elo1=10 alpha=0.05 beta=0.05
        ;;
    match)
        GAMES="${SPRT_GAMES:-200}"
        ROUNDS=$((GAMES / 2))
        echo "Running fixed-game match: tc=$TC, $GAMES games"
        echo "  HEAD vs $BASELINE_LABEL"
        "$FASTCHESS" \
            "${COMMON[@]}" \
            -rounds "$ROUNDS"
        ;;
    rating-estimate)
        GAMES="${SPRT_GAMES:-200}"
        ROUNDS=$((GAMES / 2))
        echo "Running rating-estimate match: tc=$TC, $GAMES games"
        echo "  HEAD vs $BASELINE_LABEL (Stockfish UCI_Elo=1320)"
        "$FASTCHESS" \
            "${COMMON[@]}" \
            -rounds "$ROUNDS"
        ;;
    *)
        echo "ERROR: unknown subcommand '$SUBCMD'" >&2
        usage >&2
        exit 1
        ;;
esac

echo ""
echo "PGN: $PGN"
echo "Log: $LOG"
