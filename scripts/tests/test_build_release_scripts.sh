#!/bin/bash
#
# Focused tests for the release build scripts (scripts/build-release-local.sh,
# scripts/cross-docker.sh, scripts/build-windows-zip.sh, builds/cross.just).
#
# These cover the parts that run without Docker or a remote builder:
# syntax, hard failure on unknown targets/arch labels, zip packaging
# parity with build-windows.ps1, and the orchestrator's dry-run matrix.
#
# Run: bash scripts/tests/test_build_release_scripts.sh  (or: just test-release-scripts)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

failures=0
check() {
    local label="$1"
    shift
    if "$@" >/dev/null 2>&1; then
        echo "ok   $label"
    else
        echo "FAIL $label"
        failures=$((failures + 1))
    fi
}
check_fails() {
    local label="$1"
    shift
    if "$@" >/dev/null 2>&1; then
        echo "FAIL $label (expected failure)"
        failures=$((failures + 1))
    else
        echo "ok   $label"
    fi
}

for script in scripts/build-release-local.sh scripts/cross-docker.sh scripts/build-windows-zip.sh; do
    check "bash -n $script" bash -n "$script"
done
if command -v shellcheck >/dev/null 2>&1; then
    check "shellcheck release scripts" shellcheck -S warning \
        scripts/build-release-local.sh scripts/cross-docker.sh scripts/build-windows-zip.sh
fi

# Unknown targets/labels are hard errors, never silent no-ops.
check_fails "cross-docker rejects unknown target" ./scripts/cross-docker.sh plan9-mips
check_fails "zip script rejects unknown arch" ./scripts/build-windows-zip.sh /dev/null sparc

# Zip packaging parity with build-windows.ps1 (name + root contents).
workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT
printf 'MZ dummy\n' > "$workdir/sotf-daemon.exe"
version=$(grep -m1 '^version = ' Cargo.toml | sed -E 's/version = "(.*)"/\1/')
check "zip script packages dummy exe" \
    env SOTF_DIST_DIR="$workdir/out" ./scripts/build-windows-zip.sh "$workdir/sotf-daemon.exe" x64
zip_path="$workdir/out/sotf-systemwide-${version}-windows-x64.zip"
check "zip named for workspace version" test -s "$zip_path"
listing=$(unzip -Z1 "$zip_path" | sort | paste -sd' ' -)
check "zip contains exe + README at root" test "$listing" = "README.txt sotf-daemon.exe"
check "README is versioned" \
    sh -c "unzip -p '$zip_path' README.txt | grep -q 'SotF Systemwide v$version (Windows x64)'"

# Orchestrator dry-run prints the full expected matrix and exits 0.
dry_out=$(./scripts/build-release-local.sh --dry-run 2>&1)
for artifact in \
    "sotf-systemwide-${version}-macos-arm64.pkg" \
    "sotf-systemwide-${version}-linux-arm64.AppImage" \
    "sotf-systemwide-${version}-windows-arm64.zip" \
    "sotf-systemwide-${version}-linux-x86_64.AppImage" \
    "sotf-systemwide-${version}-windows-x64.zip" \
    "sotf-systemwide_${version}_amd64.deb"; do
    check "dry-run expects $artifact" sh -c "echo \"\$0\" | grep -q '$artifact'" "$dry_out"
done

# The notarization gate in sign-macos.sh must reject payloads that are not
# Developer ID signed (the exact status-4000 failure, caught locally).
# /bin/echo is platform-signed, not Developer ID, so it must fail.
if [ "$(uname -s)" = "Darwin" ]; then
    gate_dir="$workdir/gate"
    mkdir -p "$gate_dir"
    cp /bin/echo "$gate_dir/echo"
    check "payload gate rejects non-Developer-ID binary" \
        bash -c "source scripts/sign-macos.sh >/dev/null 2>&1; ! verify_payload_signatures '$gate_dir'"
    empty_dir="$workdir/empty"
    mkdir -p "$empty_dir"
    check "payload gate passes an empty tree" \
        bash -c "source scripts/sign-macos.sh >/dev/null 2>&1; verify_payload_signatures '$empty_dir'"
fi

# The no-argument sign-macos scan must not sweep stale version variants
# (e.g. a rejected -0.7.19b- pkg) into a signing/notarization run.
check "sign-macos scan glob excludes version variants" \
    grep -qF '/*-"$VERSION"-*.pkg' scripts/sign-macos.sh

# cross.just is imported and exposes the release recipes.
if command -v just >/dev/null 2>&1; then
    check "just lists cross-arm" sh -c "just --list 2>/dev/null | grep -q 'cross-arm'"
    check "just lists release-local" sh -c "just --list 2>/dev/null | grep -q 'release-local'"
fi

if [ "$failures" -gt 0 ]; then
    echo "$failures check(s) FAILED"
    exit 1
fi
echo "All release-script checks passed"
