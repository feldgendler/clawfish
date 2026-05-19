#!/usr/bin/env sh
# scripts/corpus.sh — operator-facing wrapper around the `corpus` binary.
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
# in $OUT_DIR. The committed re-run.sh that `corpus build` writes to
# bench/corpus/ is the per-frozen-corpus operator script; this wrapper is
# the build-time helper.

set -eu

OUT_DIR="${OUT_DIR:-bench/corpus}"
SEED="${SEED:-3221225677}"  # 0xC0FFEE_0D in decimal
GAMES="${GAMES:-200}"
WORKERS="${WORKERS:-4}"
VAL_FRACTION="${VAL_FRACTION:-0.1}"
SPLIT_SEED="${SPLIT_SEED:-7}"
MAX_PLIES="${MAX_PLIES:-200}"
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

echo "corpus.sh: running deterministic self-play (seed=$SEED games=$GAMES workers=$WORKERS)"
"$CORPUS_BIN" selfplay \
    --seed "$SEED" \
    --games "$GAMES" \
    --workers "$WORKERS" \
    --out "$OUT_DIR" \
    --max-plies "$MAX_PLIES" \
    --val-fraction "$VAL_FRACTION"

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
