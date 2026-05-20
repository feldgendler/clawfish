#!/usr/bin/env sh
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

if [ -n "$GAMES" ]; then
    echo "corpus.sh: running deterministic self-play (seed=$SEED games=$GAMES workers=$WORKERS)"
    "$CORPUS_BIN" selfplay \
        --seed "$SEED" \
        --games "$GAMES" \
        --workers "$WORKERS" \
        --out "$OUT_DIR" \
        --max-plies "$MAX_PLIES" \
        --opening-random-plies "$OPENING_RANDOM_PLIES" \
        --val-fraction "$VAL_FRACTION"
else
    echo "corpus.sh: running unbounded deterministic self-play (seed=$SEED workers=$WORKERS) — Ctrl-C to stop"
    "$CORPUS_BIN" selfplay \
        --seed "$SEED" \
        --workers "$WORKERS" \
        --out "$OUT_DIR" \
        --max-plies "$MAX_PLIES" \
        --opening-random-plies "$OPENING_RANDOM_PLIES" \
        --val-fraction "$VAL_FRACTION"
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
