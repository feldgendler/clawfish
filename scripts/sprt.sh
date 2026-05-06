#!/usr/bin/env bash
# SPRT match runner using the in-process harness (`elo-iterate`).
#
# All three subcommands invoke `target/release/elo-iterate`; fastchess is no
# longer used by this script (it stays on disk only for `scripts/match.sh
# compliance`). The historical-commit baseline worktree+build flow is kept
# verbatim — only the runner inside the methodology changed.
#
# Subcommands:
#   sprt <baseline-tag>      — pentanomial-GSPRT match: HEAD vs baseline-tag
#                              (clawfish-vs-clawfish), elo0=0 elo1=10
#                              alpha=beta=0.05; up to SPRT_GAMES games (400 default).
#   match <baseline-tag>     — fixed-game-count match HEAD vs baseline-tag;
#                              200 games default.
#   rating-estimate          — fixed-game-count match HEAD vs Stockfish UCI_Elo=$STOCKFISH_ELO
#                              (default 1320 — the ADR-0012 reference point); 200 games default.
#                              No tag arg — Stockfish from PATH (brew install stockfish).
#                              CAVEAT: Stockfish UCI_Elo is calibrated by Stockfish at slow TC
#                              (typically 60+0.6 or 120+1.2). Running rating-estimate at the
#                              default `tc=10+0.1` will produce a TC-specific point estimate,
#                              not a direct CCRL-equivalent rating. SPRT_TC override available
#                              if you want to bring the TC closer to Stockfish's calibration
#                              reference at the cost of wallclock. STOCKFISH_ELO is useful for
#                              cross-validation: pit HEAD vs Stockfish at the engine's
#                              hypothesized Elo and check the score is ~50%.
#
# Usage: scripts/sprt.sh sprt baseline/random-mover
#        scripts/sprt.sh match baseline/random-mover
#        scripts/sprt.sh rating-estimate
#        SPRT_GAMES=400 scripts/sprt.sh sprt baseline/random-mover  (override)
#        SPRT_TC=10+0.1 scripts/sprt.sh sprt baseline/random-mover  (override)
#        SPRT_TC_SAMPLE='10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1' scripts/sprt.sh sprt <tag>
#                                                                    (mixed-TC per ELOH.D;
#                                                                    mutually exclusive with SPRT_TC)
#        SPRT_SEED=0xC1ABF15AE10DD005 scripts/sprt.sh sprt <tag>     (deterministic per-pair
#                                                                    TC stream)
#        SPRT_CONCURRENCY=6 scripts/sprt.sh sprt baseline/random-mover
#        SPRT_REBUILD=1 scripts/sprt.sh sprt baseline/random-mover  (force rebuild
#                                                                    of cached baseline)

set -euo pipefail

if [[ "${SPRT_DEBUG:-0}" == "1" ]]; then set -x; fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SUBCMD="${1:-}"
BASELINE_TAG="${2:-}"

usage() {
    echo "Usage: scripts/sprt.sh <sprt|match|rating-estimate> [baseline-tag]"
    echo ""
    echo "  sprt <tag>           SPRT match (elo0=0, elo1=10, alpha=0.05, beta=0.05; up to 400 games)"
    echo "  match <tag>          fixed-game-count match (200 games default)"
    echo "  rating-estimate      fixed-game-count match HEAD vs Stockfish UCI_Elo=\$STOCKFISH_ELO (200 games)"
    echo ""
    echo "Env vars:"
    echo "  STOCKFISH_ELO        default 1320; rating-estimate cross-validation knob"
    echo "  SPRT_GAMES           default sprt=400, match=200, rating-estimate=200"
    echo "  SPRT_TC              default '10+0.1'"
    echo "  SPRT_CONCURRENCY     default '6'"
    echo "  SPRT_REBUILD=1       force rebuild of cached baseline worktree"
    echo "  SPRT_DEBUG=1         enable set -x"
    echo ""
    echo "Output: target/matches/sprt/<dated>-<baseline-slug>-<subcmd>/{summary.txt,match.pgn,games/}"
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
# Build current-tree binary (incremental — fast on no-op rebuild).
# -----------------------------------------------------------------------------
echo "Building HEAD binary..."
cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml" --quiet
CURRENT_BINARY="$REPO_ROOT/target/release/clawfish"
HARNESS_BINARY="$REPO_ROOT/target/release/elo-iterate"

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
    STOCKFISH_ELO_VALUE="${STOCKFISH_ELO:-1320}"
    BASELINE_BINARY="$STOCKFISH"
    BASELINE_LABEL="stockfish-${STOCKFISH_ELO_VALUE}"
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

    # Sanity-check the baseline binary speaks UCI before the harness starts.
    # Catches "binary built with stale toolchain / corrupt cache" failures up
    # front.
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
OUT_DIR="$SPRT_DIR/${TS}-${BASELINE_SLUG}-${SUBCMD}"

# -----------------------------------------------------------------------------
# Match parameters.
# -----------------------------------------------------------------------------
TC="${SPRT_TC:-10+0.1}"
TC_SAMPLE="${SPRT_TC_SAMPLE:-}"
CONCURRENCY="${SPRT_CONCURRENCY:-6}"
SEED_ARG="${SPRT_SEED:-}"

# `--tc-sample` (mixed-TC per-pair sampling, ELOH.D) is mutually exclusive
# with `--tc`. Pick one based on whether SPRT_TC_SAMPLE is set.
if [[ -n "$TC_SAMPLE" ]]; then
    TC_HARNESS_ARGS=(--tc-sample "$TC_SAMPLE")
else
    TC_HARNESS_ARGS=(--tc "$TC")
fi

# Optional SPRT_SEED: pass through to the harness so per-pair TC sampling
# is bit-deterministic across runs (load-bearing for replays).
if [[ -n "$SEED_ARG" ]]; then
    SEED_HARNESS_ARGS=(--seed "$SEED_ARG")
else
    SEED_HARNESS_ARGS=()
fi

# Common harness invocation. Adjudication thresholds match the historical
# fastchess settings the SPRT runs were calibrated against.
COMMON_HARNESS_ARGS=(
    --engine "$CURRENT_BINARY"
    --opponent "$BASELINE_BINARY"
    --engine-launch-prefix "taskpolicy -c utility"
    --opponent-launch-prefix "taskpolicy -c utility"
    "${TC_HARNESS_ARGS[@]}"
    "${SEED_HARNESS_ARGS[@]}"
    --concurrency "$CONCURRENCY"
    --resign-movecount 3 --resign-score 600
    --draw-movenumber 34 --draw-movecount 8 --draw-score 20
    --max-moves 200
    --out-dir "$OUT_DIR"
)

# -----------------------------------------------------------------------------
# Subcommand dispatch.
# -----------------------------------------------------------------------------
if [[ -n "$TC_SAMPLE" ]]; then
    TC_LABEL="tc-sample=$TC_SAMPLE"
else
    TC_LABEL="tc=$TC"
fi

case "$SUBCMD" in
    sprt)
        GAMES="${SPRT_GAMES:-400}"
        echo "Running SPRT (in-process harness): $TC_LABEL, up to $GAMES games, elo0=0 elo1=10 alpha=0.05 beta=0.05"
        echo "  HEAD vs $BASELINE_LABEL"
        cargo run --release --bin elo-iterate --manifest-path "$REPO_ROOT/Cargo.toml" --quiet -- \
            "${COMMON_HARNESS_ARGS[@]}" \
            --max-games "$GAMES" \
            --initial-elo 0 \
            --sprt-elo0 0 --sprt-elo1 10 --sprt-alpha 0.05 --sprt-beta 0.05
        ;;
    match)
        GAMES="${SPRT_GAMES:-200}"
        echo "Running fixed-game match (in-process harness): $TC_LABEL, $GAMES games"
        echo "  HEAD vs $BASELINE_LABEL"
        cargo run --release --bin elo-iterate --manifest-path "$REPO_ROOT/Cargo.toml" --quiet -- \
            "${COMMON_HARNESS_ARGS[@]}" \
            --max-games "$GAMES" \
            --initial-elo 0 \
            --k0 0 --target-sigma 0
        ;;
    rating-estimate)
        GAMES="${SPRT_GAMES:-200}"
        echo "Running rating-estimate match: $TC_LABEL, $GAMES games (in-process harness)"
        echo "  HEAD vs $BASELINE_LABEL (Stockfish UCI_Elo=$STOCKFISH_ELO_VALUE)"
        # ELOH.B harness with --k0 0 --target-sigma 0 freezes both K and σ-stopping,
        # producing a fixed-anchor measurement equivalent to the prior fastchess
        # invocation. Per-game adjudication thresholds match scripts/match.sh defaults.
        cargo run --release --bin elo-iterate --manifest-path "$REPO_ROOT/Cargo.toml" --quiet -- \
            "${COMMON_HARNESS_ARGS[@]}" \
            --opponent-option UCI_LimitStrength=true \
            --opponent-option "UCI_Elo=$STOCKFISH_ELO_VALUE" \
            --max-games "$GAMES" \
            --initial-elo "$STOCKFISH_ELO_VALUE" \
            --k0 0 --target-sigma 0
        ;;
    *)
        echo "ERROR: unknown subcommand '$SUBCMD'" >&2
        usage >&2
        exit 1
        ;;
esac

echo ""
echo "Output dir: $OUT_DIR"
echo "  summary.txt   per-game summary + final converged: / sprt: / ci: lines"
echo "  match.pgn     concatenated PGN of all games (run-end)"
echo "  games/<N>.pgn per-game PGN files"
