#!/usr/bin/env bash
# scripts/corpus.sh — OPERATOR FRESH-BUILD wrapper around the `corpus` binary.
#
# This script is for **building (or extending) a corpus** with operator-chosen
# knobs (or env-var overrides). The natural overnight workflow:
#
#   sh scripts/corpus.sh        # runs unbounded; press Ctrl-C when satisfied
#   sh scripts/corpus.sh        # next night — RESUMES from the durable shard,
#                               # generates further games using the same SEED,
#                               # appends them; corpus grows over time.
#
# Resumability is the R1/R2/R3 invariant: the per-game CRC-framed append-block
# log is append-only with idempotent game_id resume, so a Ctrl-C / SIGTERM /
# power-loss mid-game contributes ZERO partial labels and a later run picks
# up exactly where the durable shard left off. Set `GAMES=<N>` to cap the
# run (R1: extends to game N-1 across runs); omit it (the default) for
# unbounded self-play — the manifest's `games` field records what's
# actually in the shard at exit.
#
# For **byte-identical reproduction of the vendored `bench/corpus/`
# artifact**, run `bench/corpus/re-run.sh` instead — that script reads every
# reproducibility knob from `manifest.json` (no shell-default substitution),
# so the bytes it produces match the committed corpus_sha256.
#
# Stages the canonical M6.G pipeline:
#   1) cargo build --release --bin corpus   (compile once; reused for steps 2–5)
#   2) corpus calibrate-ladder              (empirical R-TC ladder)
#   3) corpus selfplay                       (deterministic fixed-depth games)
#   4) corpus ingest-pgn (optional)          (one invocation per raw PGN)
#   5) corpus build                          (filter→quiet→dedup→cap→split)
#   6) corpus quality-gate                   (the M6.G landing gate)
#
# POSIX-sh-compatible. Inputs are env vars (defaults below); outputs land
# in $OUT_DIR.

set -eu

# Flag parsing. The script is otherwise env-var-driven; this single flag
# expresses an interactive choice ("am I about to leave this overnight?")
# that maps poorly to an env var.
BACKGROUND_QOS=1
while [ $# -gt 0 ]; do
    case "$1" in
        --no-background)
            # Overnight / full-throughput mode: default QoS, default niceness.
            # P-cores in play; UI gets sluggish.
            BACKGROUND_QOS=0
            shift
            ;;
        --help|-h)
            cat <<'USAGE'
Usage: scripts/corpus.sh [--no-background]

Flags:
  --no-background   Overnight mode: launch at default QoS + default niceness
                    (P-cores allowed, full throughput, UI gets sluggish).
                    Default: launch at background QoS + nice -n 19 (E-cores
                    preferred, ~3x slower per worker, UI stays responsive).

Most knobs are env vars: OUT_DIR, SEED, GAMES, WORKERS, VAL_FRACTION,
SPLIT_SEED, MAX_PLIES, OPENING_RANDOM_PLIES, BUCKETS, OPENING_BOOK,
BOOK_FRACTION, CORPUS_BIN. See the file header for details.
USAGE
            exit 0
            ;;
        *)
            echo "corpus.sh: unknown argument: $1 (try --help)" >&2
            exit 2
            ;;
    esac
done

OUT_DIR="${OUT_DIR:-bench/corpus}"
# Default seed matches the vendored bench/corpus/manifest.json (0xC0FFEE).
# Override via env to build a different corpus; the vendored artifact is
# byte-reproducible only via bench/corpus/re-run.sh (which reads all knobs
# from manifest.json).
SEED="${SEED:-12648430}"  # 0xC0FFEE in decimal
# Unbounded by default — kill with Ctrl-C / SIGTERM when satisfied.
# Set GAMES=<N> to cap the run; R1's idempotent resume across runs means
# repeated invocations with the same SEED and increasing N (or unbounded)
# extend the durable shard rather than re-doing already-completed games.
GAMES="${GAMES:-}"
WORKERS="${WORKERS:-1}"
VAL_FRACTION="${VAL_FRACTION:-0.1}"
SPLIT_SEED="${SPLIT_SEED:-7}"
MAX_PLIES="${MAX_PLIES:-400}"
OPENING_RANDOM_PLIES="${OPENING_RANDOM_PLIES:-8}"
BUCKETS="${BUCKETS:-100,200,400,600}"
# Opening book + mix ratio. OPENING_BOOK="" disables the book; the default
# uses the vendored bench/data/openings.epd (CC0, 40457 positions). The
# BOOK_FRACTION is the per-game coin-flip weight for book-seeded games —
# 0.5 mixes book and random-walk openings at parity (M6.H's outer simplex
# can tune it; see bench/openings.md / ADR-0035 §4).
OPENING_BOOK="${OPENING_BOOK:-bench/data/openings.epd}"
BOOK_FRACTION="${BOOK_FRACTION:-0.5}"

# Stamp the engine commit into the manifest (operator step — keeps the
# Rust binary free of a `git` subprocess dep; R5/dependency-hygiene).
CLAWFISH_COMMIT="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
export CLAWFISH_COMMIT

CORPUS_BIN="${CORPUS_BIN:-target/release/corpus}"

echo "corpus.sh: building release binary"
cargo build --release --bin corpus

mkdir -p "$OUT_DIR"

echo "corpus.sh: calibrating R-TC depth ladder (buckets=$BUCKETS)"
"$CORPUS_BIN" calibrate-ladder --buckets "$BUCKETS" --out "$OUT_DIR"

SELFPLAY_ARGS="--seed $SEED --workers $WORKERS --out $OUT_DIR \
--max-plies $MAX_PLIES --opening-random-plies $OPENING_RANDOM_PLIES \
--val-fraction $VAL_FRACTION"
if [ -n "$OPENING_BOOK" ] && [ -f "$OPENING_BOOK" ]; then
    SELFPLAY_ARGS="$SELFPLAY_ARGS --opening-book $OPENING_BOOK --book-fraction $BOOK_FRACTION"
    echo "corpus.sh: opening book = $OPENING_BOOK (book_fraction=$BOOK_FRACTION)"
elif [ -n "$OPENING_BOOK" ]; then
    echo "corpus.sh: WARNING: OPENING_BOOK=$OPENING_BOOK not found — running without book"
fi

# Background-priority launcher (gated on the --no-background flag). On
# macOS, `taskpolicy -c background` sets the process to the Background QoS
# class — biases work onto E-cores and reduces thermal pressure, so the UI
# stays responsive. `nice -n 19` is BSD-level (yields under contention) and
# is applied alongside as belt-and-suspenders.
# `taskpolicy` is macOS-only; on Linux only `nice -n 19` applies.
LAUNCH_PREFIX=""
if [ "$BACKGROUND_QOS" = "1" ]; then
    LAUNCH_PREFIX="nice -n 19"
    if command -v taskpolicy >/dev/null 2>&1; then
        LAUNCH_PREFIX="taskpolicy -c background $LAUNCH_PREFIX"
    fi
    echo "corpus.sh: launching at background QoS (UI-friendly; pass --no-background for full throughput)"
else
    echo "corpus.sh: launching at default QoS — overnight / full-throughput mode (--no-background)"
fi

if [ -n "$GAMES" ]; then
    echo "corpus.sh: running deterministic self-play (seed=$SEED games=$GAMES workers=$WORKERS)"
    # shellcheck disable=SC2086 # word-split SELFPLAY_ARGS / LAUNCH_PREFIX intentionally
    $LAUNCH_PREFIX "$CORPUS_BIN" selfplay --games "$GAMES" $SELFPLAY_ARGS
else
    echo "corpus.sh: running unbounded deterministic self-play (seed=$SEED workers=$WORKERS) — Ctrl-C to stop"
    # shellcheck disable=SC2086
    $LAUNCH_PREFIX "$CORPUS_BIN" selfplay $SELFPLAY_ARGS
fi

# (4) PGN ingestion: operator-driven. Uncomment + adapt to stage raw
#     CCRL/Lichess PGNs into target/corpus-raw/, then ingest:
#
#   "$CORPUS_BIN" ingest-pgn --path target/corpus-raw/ccrl-2022.pgn \
#       --source ccrl --out "$OUT_DIR"
#   "$CORPUS_BIN" ingest-pgn --path target/corpus-raw/lichess-2024-01.pgn \
#       --source lichess --out "$OUT_DIR"

echo "corpus.sh: building frozen corpus (split_seed=$SPLIT_SEED val_fraction=$VAL_FRACTION)"
"$CORPUS_BIN" build \
    --in "$OUT_DIR" \
    --val-fraction "$VAL_FRACTION" \
    --split-seed "$SPLIT_SEED"

echo "corpus.sh: running data-quality gate"
"$CORPUS_BIN" quality-gate --dir "$OUT_DIR"

echo "corpus.sh: done — frozen artifact at $OUT_DIR"
