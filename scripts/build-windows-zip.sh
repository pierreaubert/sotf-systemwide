#!/bin/bash
#
# Package a cross-built sotf-daemon.exe into the distribution zip:
#   dist/sotf-systemwide-<version>-windows-<arch>.zip
# containing sotf-daemon.exe + README.txt at the zip root.
#
# This is the POSIX counterpart of New-Package in scripts/build-windows.ps1
# (same zip name and README structure) for the Docker gnullvm builds driven
# by scripts/cross-docker.sh. The gnullvm exe links the Universal CRT
# dynamically (built into Windows 10/11), which is what the README states.
#
# Usage:
#   ./scripts/build-windows-zip.sh <path-to-sotf-daemon.exe> <x64|arm64>
#
# Output dir defaults to <repo>/dist; override with SOTF_DIST_DIR.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# shellcheck source=lib/common.sh
source "$SCRIPT_DIR/lib/common.sh"

EXE_PATH="${1:-}"
ARCH="${2:-}"
if [ -z "$EXE_PATH" ] || [ -z "$ARCH" ]; then
    echo "Usage: $0 <path-to-sotf-daemon.exe> <x64|arm64>" >&2
    exit 1
fi
case "$ARCH" in
    x64|arm64) ;;
    *)
        echo "ERROR: unsupported Windows arch label '$ARCH' (expected x64 or arm64)" >&2
        exit 1
        ;;
esac
if [ ! -f "$EXE_PATH" ]; then
    echo "ERROR: binary not found: $EXE_PATH" >&2
    exit 1
fi
if ! command -v zip >/dev/null 2>&1; then
    echo "ERROR: 'zip' is required to package the Windows distribution" >&2
    exit 1
fi

VERSION=$(sotf_version "$PROJECT_ROOT")
if [ -z "$VERSION" ]; then
    echo "ERROR: Could not extract version from Cargo.toml" >&2
    exit 1
fi

DIST_DIR="${SOTF_DIST_DIR:-$PROJECT_ROOT/dist}"
DIST_NAME="sotf-systemwide-${VERSION}-windows-${ARCH}"
STAGING_DIR="$DIST_DIR/.staging-${DIST_NAME}"

mkdir -p "$DIST_DIR"
rm -rf "$STAGING_DIR"
mkdir -p "$STAGING_DIR"
trap 'rm -rf "$STAGING_DIR"' EXIT

cp "$EXE_PATH" "$STAGING_DIR/sotf-daemon.exe"

cat > "$STAGING_DIR/README.txt" << EOF
SotF Systemwide v${VERSION} (Windows ${ARCH})
=========================================

System-wide audio capture, plugin-chain processing, and hardware output.

Included
--------
- sotf-daemon.exe : background daemon (cpal/WASAPI capture, JSON IPC over
  loopback TCP with a per-user port file)

Running
-------
Run sotf-daemon.exe from a terminal. It keeps running until Ctrl-C.
System-audio capture needs a virtual-cable (loopback) device; select it
with the SOTF_CAPTURE_DEVICE environment variable. Without one, the
default input device is captured. Set SOTF_SYSTEMWIDE_DRIVER=null for the
status-only fallback driver.

Requirements
------------
- Windows 10/11 ${ARCH}
  sotf-daemon.exe : needs the Universal C Runtime (included in Windows 10/11)
EOF

ZIP_PATH="$DIST_DIR/${DIST_NAME}.zip"
rm -f "$ZIP_PATH"
(cd "$STAGING_DIR" && zip -q -9 "$ZIP_PATH" sotf-daemon.exe README.txt)

sotf_log_success "Distribution created: $ZIP_PATH"
