#!/usr/bin/env bash
# EPD diagnostic suite runner — drives `epd-suite` against a HEAD or baseline
# binary on the WAC and STS corpora.
#
# Subcommands:
#   run                    — run HEAD binary against the named suite
#   backfill               — iterate over baseline tags, build each in a
#                            git-worktree, and run both suites
#
# Usage:
#   scripts/epd-suite.sh run wac
#   scripts/epd-suite.sh run sts
#   scripts/epd-suite.sh backfill            (iterates all viable baseline tags)
#   scripts/epd-suite.sh backfill <tag>      (single tag)
#
# Env vars:
#   EPD_MOVETIME    default 1000 (ms)
#   EPD_HASH        default 16 (MiB)
#   EPD_CONCURRENCY default 4
#   EPD_LIMIT       optional; first N positions only (smoke runs)
#   EPD_OUT_DIR     default target/epd-suites/<TS>/

set -euo pipefail

if [[ "${EPD_DEBUG:-0}" == "1" ]]; then set -x; fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SUBCMD="${1:-}"
ARG="${2:-}"

usage() {
    echo "Usage: scripts/epd-suite.sh <run|backfill> [arg]"
    echo ""
    echo "  run wac|sts             run HEAD binary against the named suite"
    echo "  backfill                iterate over all viable baseline tags"
    echo "  backfill <baseline-tag> single tag"
    echo ""
    echo "Env vars:"
    echo "  EPD_MOVETIME    default 1000 (ms)"
    echo "  EPD_HASH        default 16 (MiB)"
    echo "  EPD_CONCURRENCY default 4"
    echo "  EPD_LIMIT       optional first-N-positions cap (smoke)"
    echo "  EPD_OUT_DIR     default target/epd-suites/<TS>/"
}

if [[ -z "$SUBCMD" || "$SUBCMD" == "--help" || "$SUBCMD" == "-h" ]]; then
    usage
    exit 0
fi

MOVETIME="${EPD_MOVETIME:-1000}"
HASH_MIB="${EPD_HASH:-16}"
CONCURRENCY="${EPD_CONCURRENCY:-4}"
LIMIT="${EPD_LIMIT:-}"
TS="$(date +%Y%m%dT%H%M%S)"
OUT_DIR="${EPD_OUT_DIR:-$REPO_ROOT/target/epd-suites/$TS}"

WAC_EPD="$REPO_ROOT/bench/data/wac.epd"
STS_EPD="$REPO_ROOT/bench/data/sts.epd"

if [[ ! -f "$WAC_EPD" || ! -f "$STS_EPD" ]]; then
    echo "ERROR: vendored EPD data missing at $WAC_EPD or $STS_EPD" >&2
    exit 1
fi

mkdir -p "$OUT_DIR"

# Build HEAD binaries (epd-suite + clawfish).
build_head() {
    cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml" --quiet
}

# Args: <engine-binary> <suite-name> <epd-path> <output-prefix>
run_one() {
    local engine="$1"
    local suite="$2"
    local epd="$3"
    local prefix="$4"

    local args=(
        --engine "$engine"
        --suite "$suite"
        --epd "$epd"
        --movetime "$MOVETIME"
        --hash "$HASH_MIB"
        --concurrency "$CONCURRENCY"
        --output "${prefix}.txt"
    )
    if [[ -n "$LIMIT" ]]; then
        args+=(--limit "$LIMIT")
    fi

    "$REPO_ROOT/target/release/epd-suite" "${args[@]}" \
        > "${prefix}.stdout" 2> "${prefix}.stderr"
}

# Args: <baseline-tag>
build_baseline_binary() {
    local tag="$1"
    if ! git -C "$REPO_ROOT" rev-parse --verify --quiet "refs/tags/$tag" >/dev/null; then
        echo "ERROR: baseline tag '$tag' not found." >&2
        return 1
    fi

    local slug="${tag//\//-}"
    local dir="$REPO_ROOT/target/sprt-baselines/$slug"
    if [[ ! -d "$dir" ]]; then
        mkdir -p "$REPO_ROOT/target/sprt-baselines"
        git -C "$REPO_ROOT" worktree add "$dir" "$tag" >&2
    fi

    local pkg
    pkg="$(grep -E '^name *= *"' "$dir/Cargo.toml" | head -1 | sed -E 's/.*"([^"]+)".*/\1/')"
    local bin="$dir/target/release/$pkg"
    if [[ ! -x "$bin" ]]; then
        echo "Building baseline binary at $bin..." >&2
        cargo build --release --manifest-path "$dir/Cargo.toml" --quiet
    fi

    if ! printf 'uci\nquit\n' | "$bin" 2>/dev/null | grep -q '^uciok$'; then
        echo "ERROR: baseline binary at $bin did not emit 'uciok' on probe." >&2
        return 1
    fi

    echo "$bin"
}

case "$SUBCMD" in
    run)
        if [[ -z "$ARG" ]]; then usage; exit 1; fi
        case "$ARG" in
            wac) suite=wac; epd="$WAC_EPD" ;;
            sts) suite=sts; epd="$STS_EPD" ;;
            *) echo "ERROR: unknown suite '$ARG' (use wac or sts)" >&2; exit 1 ;;
        esac
        build_head
        echo "Running HEAD/$suite (movetime=${MOVETIME}ms, concurrency=$CONCURRENCY)..."
        run_one "$REPO_ROOT/target/release/clawfish" "$suite" "$epd" "$OUT_DIR/head-$suite"
        echo "Output: $OUT_DIR/head-$suite.{stdout,stderr,txt}"
        ;;
    backfill)
        build_head
        if [[ -n "$ARG" ]]; then
            tags=("$ARG")
        else
            mapfile -t tags < <(git -C "$REPO_ROOT" tag -l 'M[0-9]*' | grep -v '^M2\.E$' | sort)
        fi
        echo "Backfilling ${#tags[@]} baseline tag(s) into $OUT_DIR"
        for tag in "${tags[@]}"; do
            slug="${tag//\//-}"
            echo ""
            echo "=== $tag ==="
            if ! bin="$(build_baseline_binary "$tag")"; then
                echo "  build failed; skipping" >&2
                echo "$tag	build-failed" >> "$OUT_DIR/INDEX"
                continue
            fi
            echo "  binary: $bin"
            for suite in wac sts; do
                case "$suite" in
                    wac) epd="$WAC_EPD" ;;
                    sts) epd="$STS_EPD" ;;
                esac
                echo "  running $suite..."
                if ! run_one "$bin" "$suite" "$epd" "$OUT_DIR/${slug}-${suite}"; then
                    echo "    $suite: harness exited non-zero — see $OUT_DIR/${slug}-${suite}.stderr" >&2
                    echo "$tag	$suite	harness-error" >> "$OUT_DIR/INDEX"
                    continue
                fi
                # Extract a one-line digest from the summary for the INDEX file.
                # WAC: `solved: X/Y` and `score: P%`. STS: `total credit: X/Y` and `score: P%`.
                if [[ "$suite" == "wac" ]]; then
                    digest="$(awk '/^  solved:/ {s=$2} /^  score:/ {p=$2} END {print "solved="s" score="p}' "$OUT_DIR/${slug}-${suite}.stdout")"
                else
                    digest="$(awk '/^  total credit:/ {s=$3} /^  score:/ {p=$2} /^  STS-Elo/ {e=$3} END {print "credit="s" score="p" elo="e}' "$OUT_DIR/${slug}-${suite}.stdout")"
                fi
                echo "$tag	$suite	$digest" >> "$OUT_DIR/INDEX"
            done
        done
        echo ""
        echo "Backfill complete. INDEX:"
        cat "$OUT_DIR/INDEX"
        ;;
    *)
        usage; exit 1 ;;
esac
