#!/usr/bin/env bash
# Install the pinned fastchess release to vendor/fastchess/fastchess.
#
# Required for `scripts/match.sh compliance` only — all other harness flows
# (sprt, match, self-play, vs-stockfish, rating-estimate) use the in-process
# `elo-iterate` binary as of ELOH.E (see docs/decisions/0022-eloh-sprt-mechanics.md).
#
# Idempotent: re-running when the binary is already present prints a note and
# exits 0 without downloading or verifying again.
#
# Usage: scripts/install-fastchess.sh
# Targets: macOS arm64 only (the project's primary platform per ADR-0002).

set -euo pipefail

# -------------------------------------------------------------------------
# Pinned release constants.  Three-line edit to bump the version:
#   EXPECTED_RELEASE_TAG, EXPECTED_VERSION_LINE, EXPECTED_SHA256.
# -------------------------------------------------------------------------
EXPECTED_RELEASE_TAG="v1.8.0-alpha"
EXPECTED_VERSION_LINE="alpha 1.8.0"
EXPECTED_SHA256="5f5a313b8f8d6222a9914ba76f000197e3bfb3c919a12b9e92e6ac5d516b91fc"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VENDOR_DIR="$REPO_ROOT/vendor/fastchess"
BINARY="$VENDOR_DIR/fastchess"
TARBALL="${TMPDIR:-/tmp}/fastchess-mac-arm64.tar"
DOWNLOAD_URL="https://github.com/Disservin/fastchess/releases/download/$EXPECTED_RELEASE_TAG/fastchess-mac-arm64.tar"

# -------------------------------------------------------------------------
# Pre-flight: platform + tool guards.
# -------------------------------------------------------------------------
if [[ "$(uname -sm)" != "Darwin arm64" ]]; then
    echo "ERROR: this script targets macOS arm64 only; detected: $(uname -sm)" >&2
    exit 1
fi
if ! command -v curl >/dev/null 2>&1; then
    echo "ERROR: curl not on PATH (preinstalled on macOS; did something nuke it?)" >&2
    exit 1
fi

# -------------------------------------------------------------------------
# Pre-flight: idempotent check — skip if already installed.
# -------------------------------------------------------------------------
if "$BINARY" --version 2>/dev/null | grep -q "$EXPECTED_VERSION_LINE"; then
    echo "fastchess $EXPECTED_VERSION_LINE already installed at $BINARY"
    exit 0
fi

# -------------------------------------------------------------------------
# Download.
# -------------------------------------------------------------------------
echo "Downloading fastchess $EXPECTED_RELEASE_TAG from GitHub..."
curl -fsSL "$DOWNLOAD_URL" -o "$TARBALL"

# -------------------------------------------------------------------------
# SHA256 verification.  Hard-fail with both values if mismatch.
# -------------------------------------------------------------------------
actual_sha="$(shasum -a 256 "$TARBALL" | awk '{print $1}')"
if [[ "$actual_sha" != "$EXPECTED_SHA256" ]]; then
    echo "ERROR: SHA256 mismatch for $TARBALL" >&2
    echo "  expected: $EXPECTED_SHA256" >&2
    echo "  actual:   $actual_sha" >&2
    exit 1
fi
echo "SHA256 verified: $actual_sha"

# -------------------------------------------------------------------------
# Extract.  The tarball contents land in fastchess-mac-arm64/ inside TMPDIR;
# copy the three relevant files into vendor/fastchess/.
# -------------------------------------------------------------------------
mkdir -p "$VENDOR_DIR"
EXTRACT_DIR="${TMPDIR:-/tmp}"
tar -xf "$TARBALL" -C "$EXTRACT_DIR"
cp "$EXTRACT_DIR/fastchess-mac-arm64/fastchess"   "$VENDOR_DIR/fastchess"
cp "$EXTRACT_DIR/fastchess-mac-arm64/LICENSE"     "$VENDOR_DIR/LICENSE"
cp "$EXTRACT_DIR/fastchess-mac-arm64/README.md"   "$VENDOR_DIR/README.md"
chmod +x "$BINARY"

# -------------------------------------------------------------------------
# Post-install version verification.
# -------------------------------------------------------------------------
if ! "$BINARY" --version | grep -q "$EXPECTED_VERSION_LINE"; then
    actual_ver="$("$BINARY" --version 2>&1 || true)"
    echo "ERROR: version mismatch after install." >&2
    echo "  expected to contain: $EXPECTED_VERSION_LINE" >&2
    echo "  binary reported:     $actual_ver" >&2
    exit 1
fi

echo "fastchess $EXPECTED_VERSION_LINE installed at $BINARY"
