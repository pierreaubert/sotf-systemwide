#!/bin/bash
#
# Build one non-macOS release target in Docker.
#
# Mirrors the docker-* recipes in ../sotf/builds/cross.just, adapted to
# sotf-systemwide: the Dockerfiles (builds/docker/) install tools only,
# then cargo runs with the whole all_of_sotf aggregate tree mounted at
# /workspace so the ../sotf and ../sotf-daw path dependencies resolve.
# Artifacts land in <repo>/dist/ through the mount.
#
# Usage:
#   ./scripts/cross-docker.sh <target> [--skip-image-build]
#
# Targets:
#   linux-arm64     AppImage + tarball + .deb (aarch64, GNU)
#   linux-x86       AppImage + tarball + .deb (x86_64, GNU)
#   windows-arm64   dist zip with sotf-daemon.exe (aarch64-pc-windows-gnullvm)
#   windows-x86     dist zip with sotf-daemon.exe (x86_64-pc-windows-gnullvm)
#
# Environment:
#   SOTF_AGGREGATE_ROOT   Aggregate tree to mount (default: repo parent)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
AGGREGATE_ROOT="${SOTF_AGGREGATE_ROOT:-$(cd "$PROJECT_ROOT/.." && pwd)}"
DOCKER_DIR="$PROJECT_ROOT/builds/docker"

# shellcheck source=lib/common.sh
source "$SCRIPT_DIR/lib/common.sh"

TARGET="${1:-}"
SKIP_IMAGE_BUILD=false
case "${2:-}" in
    "") ;;
    --skip-image-build) SKIP_IMAGE_BUILD=true ;;
    *)
        echo "Unknown option: $2" >&2
        exit 1
        ;;
esac

if ! command -v docker >/dev/null 2>&1; then
    sotf_log_error "docker is required for cross builds"
    exit 1
fi
# BuildKit (buildx) is required: the legacy builder ignores --platform
# when the base image is already pulled for another architecture, which
# silently cross-builds for the wrong arch under binfmt.
if ! docker buildx version >/dev/null 2>&1; then
    sotf_log_error "docker buildx is required (install the buildx CLI plugin: https://github.com/docker/buildx#installing)"
    exit 1
fi

# Per-target configuration. Unknown targets are a hard error.
case "$TARGET" in
    linux-arm64)
        PLATFORM="linux/arm64"
        IMAGE="sotf-systemwide-linux-arm64"
        DOCKERFILE="Dockerfile.linux-gnu"
        ;;
    linux-x86)
        PLATFORM="linux/amd64"
        IMAGE="sotf-systemwide-linux-x86"
        DOCKERFILE="Dockerfile.linux-gnu"
        ;;
    windows-arm64)
        PLATFORM="linux/arm64"
        IMAGE="sotf-systemwide-win-arm64"
        DOCKERFILE="Dockerfile.windows-gnullvm"
        WINDOWS_TARGET="aarch64-pc-windows-gnullvm"
        WINDOWS_ARCH_LABEL="arm64"
        ;;
    windows-x86)
        PLATFORM="linux/amd64"
        IMAGE="sotf-systemwide-win-x86"
        DOCKERFILE="Dockerfile.windows-gnullvm"
        WINDOWS_TARGET="x86_64-pc-windows-gnullvm"
        WINDOWS_ARCH_LABEL="x64"
        ;;
    *)
        echo "Usage: $0 <linux-arm64|linux-x86|windows-arm64|windows-x86> [--skip-image-build]" >&2
        exit 1
        ;;
esac

TARGET_VOLUME="sotf-systemwide-target-$TARGET"

if ! $SKIP_IMAGE_BUILD; then
    sotf_log_info "Building Docker image $IMAGE ($PLATFORM)..."
    # The Dockerfiles copy nothing from the build context, so the context
    # is just builds/docker — never the repo (target/ would be uploaded).
    build_args=()
    if [ -n "${WINDOWS_TARGET:-}" ]; then
        build_args+=(--build-arg "WINDOWS_TARGET=$WINDOWS_TARGET")
        # llvm-mingw tarball cache (tools/mingw-cache/, may be empty):
        # the Windows Dockerfile bind-mounts it and only downloads from
        # GitHub when no cached tarball is present.
        mkdir -p "$PROJECT_ROOT/tools/mingw-cache"
        build_args+=(--build-context "mingwcache=$PROJECT_ROOT/tools/mingw-cache")
    fi
    # ${arr[@]+...} keeps an empty array legal under nounset on bash 3.2.
    # --load: with a docker-container builder the result otherwise stays
    # in the build cache and `docker run` cannot find the tag.
    docker buildx build --load --platform "$PLATFORM" -t "$IMAGE" \
        ${build_args[@]+"${build_args[@]}"} \
        -f "$DOCKER_DIR/$DOCKERFILE" "$DOCKER_DIR"
fi

mkdir -p "$PROJECT_ROOT/dist"

case "$TARGET" in
    linux-*)
        sotf_log_info "Building Linux artifacts in $IMAGE..."
        docker run --rm --platform "$PLATFORM" \
            -v "$AGGREGATE_ROOT":/workspace \
            -v "$TARGET_VOLUME":/build-target \
            -v sotf-systemwide-cargo-registry-linux:/usr/local/cargo/registry \
            -v sotf-systemwide-cargo-git-linux:/usr/local/cargo/git \
            -w /workspace/sotf-systemwide \
            -e CARGO_HOME=/usr/local/cargo \
            -e CARGO_TARGET_DIR=/build-target \
            -e CARGO_INCREMENTAL=0 \
            "$IMAGE" ./scripts/build-linux.sh --appimage --deb
        ;;
    windows-*)
        # Target-specific compiler/linker environment, as ENV lines in
        # sotf's per-target Dockerfiles. Passed at run time because this
        # image is shared between both Windows targets.
        underscored=$(echo "$WINDOWS_TARGET" | tr '-' '_')
        upper=$(echo "$underscored" | tr '[:lower:]' '[:upper:]')
        mingw_prefix="${WINDOWS_TARGET%%-*}-w64-mingw32"
        sotf_log_info "Building $WINDOWS_TARGET in $IMAGE..."
        docker run --rm --platform "$PLATFORM" \
            -v "$AGGREGATE_ROOT":/workspace \
            -v "$TARGET_VOLUME":/build-target \
            -v sotf-systemwide-cargo-registry-windows:/root/.cargo/registry \
            -v sotf-systemwide-cargo-git-windows:/root/.cargo/git \
            -w /workspace/sotf-systemwide \
            -e CARGO_TARGET_DIR=/build-target \
            -e CARGO_INCREMENTAL=0 \
            -e "CC_${underscored}=${mingw_prefix}-clang" \
            -e "CXX_${underscored}=${mingw_prefix}-clang++" \
            -e "AR_${underscored}=${mingw_prefix}-ar" \
            -e "CARGO_TARGET_${upper}_LINKER=${mingw_prefix}-clang" \
            "$IMAGE" bash -c "cargo build --profile dist --target $WINDOWS_TARGET -p sotf-daemon --bin sotf-daemon && ./scripts/build-windows-zip.sh /build-target/$WINDOWS_TARGET/dist/sotf-daemon.exe $WINDOWS_ARCH_LABEL"
        ;;
esac

sotf_log_success "Done ($TARGET). Output in: $PROJECT_ROOT/dist/"
