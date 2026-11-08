#!/bin/bash
#
# Build script for the SotF systemwide daemon on Linux
# Creates a distributable tarball, and optionally an AppImage and a .deb.
#
# Usage:
#   ./build-linux.sh                    # Build release binary + tarball (native Linux)
#   ./build-linux.sh --appimage         # Also create an AppImage (native Linux only)
#   ./build-linux.sh --deb              # Also create a .deb package (Debian/Ubuntu)
#   ./build-linux.sh --install-tools    # Download and install linuxdeploy
#   ./build-linux.sh --appimage --deb   # All three artifacts (mirrors ../sotf)
#
# Cross-compiling from macOS is not supported by this script: use the Docker
# pattern from ../sotf (builds/cross.just), mounting the whole all_of_sotf
# tree so the ../sotf-daw path dependencies resolve, then run this script
# natively inside the container.
#
# Prerequisites:
#   - Rust toolchain
#   - libasound2-dev + pkg-config (cpal/ALSA capture; see
#     builds/systemwide/Dockerfile.test-linux-arm64)
#   - For AppImage: linuxdeploy (auto-downloaded if missing, or use --install-tools)
#   - For .deb: dpkg-deb (usually pre-installed on Debian/Ubuntu)
#

set -euo pipefail

# Configuration
# APP_NAME is used as the dist filename prefix (lowercase, hyphenated).
# BINARY_NAME must match the [[bin]] name in crates/daemon/Cargo.toml.
APP_NAME="sotf-systemwide"
BINARY_NAME="sotf-daemon"
PACKAGE_NAME="sotf-daemon"
SERVICE_NAME="sotf-daemon.service"
DESKTOP_NAME="org.spinorama.sotf-daemon"

# Paths
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# shellcheck source=lib/common.sh
source "$SCRIPT_DIR/lib/common.sh"
source "$PROJECT_ROOT/../sotf-daw/scripts/lib/nnnoiseless_model_notices.sh"

# Extract version from root Cargo.toml
VERSION=$(sotf_version "$PROJECT_ROOT")
if [ -z "$VERSION" ]; then
    echo "ERROR: Could not extract version from Cargo.toml"
    exit 1
fi

# Build options
CREATE_APPIMAGE=false
CREATE_DEB=false
INSTALL_TOOLS=false
CLEAN=false
TARGET=""
BUILD_DIR=""
DIST_DIR="$PROJECT_ROOT/dist"
TOOLS_DIR="$PROJECT_ROOT/tools"
ASSETS_DIR="$PROJECT_ROOT/builds/linux"

# The daemon has no font/toolkit notice payloads (unlike the GPUI app); the
# only staged third-party notices are the nnnoiseless model ones, matching
# what scripts/build-systemwide.sh embeds in the macOS bundle.
validate_linux_notice_sources() {
    validate_nnnoiseless_model_notice_sources "$PROJECT_ROOT/../sotf-daw"
}

stage_linux_notices() {
    local destination="$1"
    validate_linux_notice_sources || return 1
    stage_nnnoiseless_model_notices "$PROJECT_ROOT/../sotf-daw" "$destination"
}

# linuxdeploy tool configuration
LINUXDEPLOY_VERSION="continuous"
LINUXDEPLOY_BIN=""

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --appimage)
            CREATE_APPIMAGE=true
            shift
            ;;
        --deb)
            CREATE_DEB=true
            shift
            ;;
        --install-tools)
            INSTALL_TOOLS=true
            shift
            ;;
        --clean)
            CLEAN=true
            shift
            ;;
        --help|-h)
            echo "Usage: $0 [options]"
            echo ""
            echo "Options:"
            echo "  --appimage       Create AppImage (native Linux only)"
            echo "  --deb            Create .deb package (Debian/Ubuntu)"
            echo "  --install-tools  Download and install linuxdeploy"
            echo "  --clean          Clean build directory before building"
            echo "  --help           Show this help message"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# Set build directory based on CARGO_TARGET_DIR / CARGO_BUILD_TARGET.
# Release cuts use the `dist` profile (fat LTO + codegen-units=1) — see
# [profile.dist] in the root Cargo.toml. Cargo emits to .../dist/ instead of
# .../release/. A Docker/CARGO_BUILD_TARGET triple keeps its subdirectory.
TARGET_DIR="${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}"
if [ -n "${CARGO_BUILD_TARGET:-}" ]; then
    # When CARGO_BUILD_TARGET is set (e.g. inside Docker), binaries go to <target_dir>/<triple>/dist
    TARGET="$CARGO_BUILD_TARGET"
    BUILD_DIR="$TARGET_DIR/$TARGET/dist"
else
    BUILD_DIR="$TARGET_DIR/dist"
fi

# Color output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

log_warning() {
    echo -e "${YELLOW}[WARNING]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# Download and install linuxdeploy
install_linuxdeploy() {
    log_info "Installing linuxdeploy..."

    # Detect architecture
    local arch
    arch=$(uname -m)
    local tool_url

    case "$arch" in
        x86_64)
            tool_url="https://github.com/linuxdeploy/linuxdeploy/releases/download/${LINUXDEPLOY_VERSION}/linuxdeploy-x86_64.AppImage"
            ;;
        aarch64|arm64)
            tool_url="https://github.com/linuxdeploy/linuxdeploy/releases/download/${LINUXDEPLOY_VERSION}/linuxdeploy-aarch64.AppImage"
            ;;
        *)
            log_error "Unsupported architecture for linuxdeploy: $arch"
            log_info "Please download linuxdeploy manually from:"
            log_info "  https://github.com/linuxdeploy/linuxdeploy/releases"
            exit 1
            ;;
    esac

    mkdir -p "$TOOLS_DIR"

    local tool_path="$TOOLS_DIR/linuxdeploy"

    log_info "Downloading linuxdeploy for $arch..."
    log_info "URL: $tool_url"

    # Large release assets are truncated on some networks (curl 18 / short
    # reads), so download with resume: each pass appends to any partial file
    # (including stale ones from previous failed runs) until the transfer is
    # reported complete. The partial file lives in $TOOLS_DIR, so a retry in
    # a later run resumes instead of restarting.
    local downloader=""
    if command -v curl &> /dev/null; then
        downloader="curl"
    elif command -v wget &> /dev/null; then
        downloader="wget"
    else
        log_error "Neither curl nor wget found. Please install one of them."
        exit 1
    fi

    local pass=1
    local max_passes=40
    local complete=""
    while [ "$pass" -le "$max_passes" ]; do
        if [ "$downloader" = "curl" ]; then
            if curl -L -C - --retry 3 --retry-all-errors -o "$tool_path" "$tool_url"; then
                complete="1"
                break
            fi
        else
            if wget -c -O "$tool_path" "$tool_url"; then
                complete="1"
                break
            fi
        fi
        log_info "Download pass $pass incomplete, resuming..."
        pass=$((pass + 1))
        sleep 5
    done
    if [ -z "$complete" ]; then
        log_error "linuxdeploy download did not complete after $max_passes passes (partial kept in $tool_path, will resume on retry)"
        exit 1
    fi

    chmod +x "$tool_path"

    if [ ! -f "$tool_path" ] || [ ! -s "$tool_path" ]; then
        log_error "Download failed or file is empty"
        exit 1
    fi

    # Verify the download by running it: a truncated AppImage fails here
    # instead of later at package time.
    local plugins
    if [ -f /.dockerenv ] || [ ! -e /dev/fuse ]; then
        plugins=$(APPIMAGE_EXTRACT_AND_RUN=1 "$tool_path" --list-plugins 2>&1) || {
            log_error "Downloaded linuxdeploy is not functional (truncated download?)"
            rm -f "$tool_path"
            exit 1
        }
    else
        plugins=$("$tool_path" --list-plugins 2>&1) || {
            log_error "Downloaded linuxdeploy is not functional (truncated download?)"
            rm -f "$tool_path"
            exit 1
        }
    fi
    log_info "linuxdeploy plugins: $plugins"

    log_success "linuxdeploy installed to $tool_path"
}

run_linuxdeploy() {
    if [ -f /.dockerenv ] || [ ! -e /dev/fuse ]; then
        APPIMAGE_EXTRACT_AND_RUN=1 "$LINUXDEPLOY_BIN" "$@"
    else
        "$LINUXDEPLOY_BIN" "$@"
    fi
}

# Check prerequisites
check_prerequisites() {
    log_info "Checking prerequisites..."

    if ! command -v cargo &> /dev/null; then
        log_error "Rust/Cargo is not installed"
        exit 1
    fi

    if $CREATE_APPIMAGE; then
        # Check for linuxdeploy in PATH or in tools directory, auto-download if missing
        if command -v linuxdeploy &> /dev/null; then
            LINUXDEPLOY_BIN="linuxdeploy"
        elif [ -x "$TOOLS_DIR/linuxdeploy" ]; then
            LINUXDEPLOY_BIN="$TOOLS_DIR/linuxdeploy"
            log_info "Using linuxdeploy from $TOOLS_DIR"
        else
            log_info "linuxdeploy not found, downloading automatically..."
            install_linuxdeploy
            LINUXDEPLOY_BIN="$TOOLS_DIR/linuxdeploy"
        fi
    fi

    if $CREATE_DEB; then
        if ! command -v dpkg-deb &> /dev/null; then
            log_warning "dpkg-deb not found. Will skip .deb creation."
            log_info "Install with: sudo apt install dpkg"
            CREATE_DEB=false
        fi
        if ! command -v fakeroot &> /dev/null; then
            log_warning "fakeroot not found (recommended for .deb creation)"
            log_info "Install with: sudo apt install fakeroot"
        fi
    fi

    log_success "Prerequisites check passed"
}

# Clean build artifacts
clean_build() {
    if $CLEAN; then
        log_info "Cleaning build directory..."
        cargo clean -p "$PACKAGE_NAME"
    fi
}

# Build the binary
build_binary() {
    log_info "Building release binary..."

    cd "$PROJECT_ROOT"

    # No --features hal here: driver-hal is macOS-only (see
    # target.'cfg(target_os = "macos")' in crates/daemon/Cargo.toml). On Linux
    # the daemon captures through the cpal-backed driver with NullDriver as
    # the status-only fallback.
    log_info "Building native Linux binary..."
    cargo build --profile dist --package "$PACKAGE_NAME"

    if [ ! -f "$BUILD_DIR/$BINARY_NAME" ]; then
        log_error "Binary not found at $BUILD_DIR/$BINARY_NAME"
        exit 1
    fi

    log_success "Daemon binary built successfully"
}

# Create distribution tarball (always built)
create_tarball() {
    log_info "Creating distribution tarball..."

    # Normalize arch label to match the dist artifacts (arm64/x86_64).
    local arch
    case "$(uname -m)" in
        x86_64)
            arch="x86_64"
            ;;
        aarch64|arm64)
            arch="arm64"
            ;;
        *)
            arch="$(uname -m)"
            ;;
    esac

    local tarball_name="${APP_NAME}-${VERSION}-linux-${arch}"
    local staging_dir="$DIST_DIR/$tarball_name"

    mkdir -p "$DIST_DIR"
    rm -rf "$staging_dir"
    mkdir -p "$staging_dir"

    # Daemon binary
    cp "$BUILD_DIR/$BINARY_NAME" "$staging_dir/"
    chmod +x "$staging_dir/$BINARY_NAME"

    # systemd user unit
    if [ ! -f "$ASSETS_DIR/$SERVICE_NAME" ]; then
        log_error "Service file not found: $ASSETS_DIR/$SERVICE_NAME"
        exit 1
    fi
    cp "$ASSETS_DIR/$SERVICE_NAME" "$staging_dir/"

    # Third-party notices, next to the payload like the macOS bundle's
    # Resources directory.
    stage_linux_notices "$staging_dir/share/doc/sotf"

    # Install script for user-level install (binary + systemd user unit)
    cat > "$staging_dir/install-daemon.sh" << 'INSTALL_EOF'
#!/bin/sh
# Install sotf-daemon as a systemd user service.
# Run as: ./install-daemon.sh [--uninstall]
set -e

BIN_DIR="$HOME/.local/bin"
UNIT_DIR="$HOME/.config/systemd/user"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

if [ "$1" = "--uninstall" ]; then
    systemctl --user disable --now sotf-daemon.service 2>/dev/null || true
    rm -f "$UNIT_DIR/sotf-daemon.service"
    rm -f "$BIN_DIR/sotf-daemon"
    systemctl --user daemon-reload 2>/dev/null || true
    echo "sotf-daemon uninstalled."
    exit 0
fi

mkdir -p "$BIN_DIR" "$UNIT_DIR"

cp "$SCRIPT_DIR/sotf-daemon" "$BIN_DIR/sotf-daemon"
chmod +x "$BIN_DIR/sotf-daemon"
cp "$SCRIPT_DIR/sotf-daemon.service" "$UNIT_DIR/sotf-daemon.service"

systemctl --user daemon-reload 2>/dev/null || true
echo "sotf-daemon installed to $BIN_DIR/sotf-daemon"
echo "Enable with: systemctl --user enable --now sotf-daemon.service"
INSTALL_EOF
    chmod +x "$staging_dir/install-daemon.sh"

    # Create a simple README
    cat > "$staging_dir/README.txt" << EOF
$APP_NAME v${VERSION}
========================

System-wide audio processing daemon: captures system audio, runs it through
the SOTF plugin chain, and outputs to physical devices. External GUIs control
it over a Unix socket with JSON messages (one object per line).

Running
-------
./sotf-daemon

Install as a systemd user service
----------------------------------
Run the install script:

    ./install-daemon.sh

This installs the binary to ~/.local/bin and the unit to
~/.config/systemd/user, then prints the systemctl command to enable it.

To uninstall:

    ./install-daemon.sh --uninstall

Requirements
------------
- Linux x86_64 or ARM64
- PipeWire (or PulseAudio) for audio capture/playback
- systemd user session for the service unit

For more information, visit: https://github.com/pierreaubert/all_of_sotf
EOF

    # Create tarball
    cd "$DIST_DIR"
    tar -czvf "${tarball_name}.tar.gz" "$tarball_name"
    rm -rf "$staging_dir"

    log_success "Tarball created: $DIST_DIR/${tarball_name}.tar.gz"
}

# Create AppImage (native Linux only)
create_appimage() {
    if ! $CREATE_APPIMAGE; then
        return
    fi

    log_info "Creating AppImage..."

    # Stage the AppDir on a local filesystem, not in $DIST_DIR: linuxdeploy's
    # copy step fails with "Permission denied" when the AppDir lives on a
    # Docker Desktop (virtiofs) bind mount, and the AppDir is transient —
    # only $OUTPUT (written separately below) belongs in $DIST_DIR.
    local appdir_parent
    appdir_parent=$(mktemp -d)
    local appdir="$appdir_parent/${APP_NAME}.AppDir"

    # Use the canonical desktop file from builds/linux/
    local desktop_file="$ASSETS_DIR/${DESKTOP_NAME}.desktop"
    if [ ! -f "$desktop_file" ]; then
        log_error "Desktop file not found: $desktop_file"
        exit 1
    fi

    # Prepare icon when one is shipped in builds/linux/
    local icon_file=""
    if [ -f "$ASSETS_DIR/sotf-daemon.png" ]; then
        local resized_icon="$DIST_DIR/sotf-daemon.png"
        if command -v convert &> /dev/null; then
            convert "$ASSETS_DIR/sotf-daemon.png" -resize 256x256 "$resized_icon"
            icon_file="$resized_icon"
        elif command -v ffmpeg &> /dev/null; then
            ffmpeg -y -i "$ASSETS_DIR/sotf-daemon.png" -vf scale=256:256 "$resized_icon" 2>/dev/null
            icon_file="$resized_icon"
        else
            cp "$ASSETS_DIR/sotf-daemon.png" "$resized_icon"
            icon_file="$resized_icon"
        fi
    else
        log_warning "No icon in $ASSETS_DIR/sotf-daemon.png; building AppImage without one"
    fi

    # Normalize arch label to match the rest of the dist artifacts (arm64/x86_64,
    # not aarch64).
    local arch
    case "$(uname -m)" in
        aarch64|arm64) arch="arm64" ;;
        x86_64)        arch="x86_64" ;;
        *)             arch="$(uname -m)" ;;
    esac
    export OUTPUT="$DIST_DIR/${APP_NAME}-${VERSION}-linux-${arch}.AppImage"

    # Prefer a cached type2 runtime (tools/runtime-<arch>): appimagetool
    # otherwise downloads it from GitHub on every run, and that download
    # 504s often enough to fail release builds. Extract one from any
    # existing AppImage with:
    #   dd if=<app>.AppImage of=tools/runtime-<arch> bs=1 \
    #      count=$(<app>.AppImage --appimage-offset)
    local runtime_file=""
    case "$arch" in
        x86_64) runtime_file="$TOOLS_DIR/runtime-x86_64" ;;
        arm64)  runtime_file="$TOOLS_DIR/runtime-aarch64" ;;
    esac
    if [ -n "$runtime_file" ] && [ -f "$runtime_file" ]; then
        export LDAI_RUNTIME_FILE="$runtime_file"
        log_info "Using cached AppImage runtime: $runtime_file"
    fi

    local deploy_args=(
        --appdir "$appdir"
        --executable "$BUILD_DIR/$BINARY_NAME"
        --desktop-file "$desktop_file"
        --output appimage
    )

    if [ -n "$icon_file" ]; then
        deploy_args+=(--icon-file "$icon_file")
    fi

    # Stage notices before the single linuxdeploy pass so they land in the
    # final image. (--executable populates the AppDir on this same pass.)
    mkdir -p "$appdir/usr/share/doc/sotf"
    stage_linux_notices "$appdir/usr/share/doc/sotf"

    run_linuxdeploy "${deploy_args[@]}"

    rm -rf "$appdir_parent"
    log_success "AppImage created: $OUTPUT"
}

# Create .deb package (Debian/Ubuntu)
create_deb() {
    if ! $CREATE_DEB; then
        return
    fi

    log_info "Creating .deb package..."

    # Determine architecture
    local arch
    local deb_arch
    arch=$(uname -m)
    case "$arch" in
        x86_64)
            deb_arch="amd64"
            ;;
        aarch64|arm64)
            deb_arch="arm64"
            ;;
        *)
            deb_arch="$arch"
            ;;
    esac

    local deb_name="sotf-systemwide_${VERSION}_${deb_arch}"
    local deb_dir="$DIST_DIR/${deb_name}"

    rm -rf "$deb_dir"
    mkdir -p "$deb_dir/DEBIAN"
    mkdir -p "$deb_dir/usr/bin"
    mkdir -p "$deb_dir/usr/lib/systemd/user"
    mkdir -p "$deb_dir/usr/share/doc/sotf"

    # Daemon binary
    cp "$BUILD_DIR/$BINARY_NAME" "$deb_dir/usr/bin/sotf-daemon"
    chmod 755 "$deb_dir/usr/bin/sotf-daemon"

    # systemd user unit
    if [ ! -f "$ASSETS_DIR/$SERVICE_NAME" ]; then
        log_error "Service file not found: $ASSETS_DIR/$SERVICE_NAME"
        exit 1
    fi
    cp "$ASSETS_DIR/$SERVICE_NAME" "$deb_dir/usr/lib/systemd/user/$SERVICE_NAME"
    chmod 644 "$deb_dir/usr/lib/systemd/user/$SERVICE_NAME"

    # Third-party notices
    stage_linux_notices "$deb_dir/usr/share/doc/sotf"

    # Create copyright file
    cat > "$deb_dir/usr/share/doc/sotf/copyright" << EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: SotF Systemwide
Source: https://github.com/pierreaubert/all_of_sotf

Files: *
Copyright: $(date +%Y) SotF Authors
License: GPL-3.0-or-later
EOF

    # Create changelog (minimal)
    cat > "$deb_dir/usr/share/doc/sotf/changelog" << EOF
sotf-systemwide (${VERSION}) stable; urgency=medium

  * Release ${VERSION}

 -- SotF Team <sotf@example.com>  $(date -R)
EOF
    gzip -9 -n "$deb_dir/usr/share/doc/sotf/changelog"

    # Calculate installed size (in KB)
    local installed_size
    installed_size=$(du -sk "$deb_dir" | cut -f1)

    # Create control file
    cat > "$deb_dir/DEBIAN/control" << EOF
Package: sotf-systemwide
Version: ${VERSION}
Section: sound
Priority: optional
Architecture: ${deb_arch}
Installed-Size: ${installed_size}
Depends: libasound2 (>= 1.0.16)
Recommends: pipewire | pulseaudio
Maintainer: SotF Team <sotf@example.com>
Homepage: https://github.com/pierreaubert/all_of_sotf
Description: System-wide audio processing daemon
 SotF Systemwide captures system audio, runs it through the SOTF plugin
 chain, and outputs to physical devices. External GUIs control it over a
 Unix socket with JSON messages.
 .
 After installing, enable the user service with:
   systemctl --user enable --now sotf-daemon.service
EOF

    # Normalize permissions (staging inherits the caller's umask; lintian
    # expects 0755 dirs and 0644 doc files).
    find "$deb_dir" -type d -exec chmod 755 {} +
    find "$deb_dir/usr/share/doc" -type f -exec chmod 644 {} +

    # Build the .deb package
    local deb_file="$DIST_DIR/${deb_name}.deb"

    if command -v fakeroot &> /dev/null; then
        fakeroot dpkg-deb --build "$deb_dir" "$deb_file"
    else
        dpkg-deb --build "$deb_dir" "$deb_file"
    fi

    # Clean up
    rm -rf "$deb_dir"

    # Verify the package
    if command -v lintian &> /dev/null; then
        log_info "Running lintian checks..."
        lintian "$deb_file" || true
    fi

    log_success ".deb package created: $deb_file"
}

# Main build process
main() {
    # Handle --install-tools first (standalone operation)
    if $INSTALL_TOOLS; then
        log_info "=========================================="
        log_info "Installing AppImage Tools"
        log_info "=========================================="
        install_linuxdeploy
        log_success "Tools installation complete!"
        exit 0
    fi

    validate_linux_notice_sources

    log_info "=========================================="
    log_info "Building $APP_NAME v$VERSION for Linux"
    log_info "=========================================="

    check_prerequisites
    clean_build
    build_binary
    create_tarball

    if $CREATE_APPIMAGE; then
        create_appimage
    fi

    if $CREATE_DEB; then
        create_deb
    fi

    log_info "=========================================="
    log_success "Build complete!"
    log_info "=========================================="

    local arch
    local deb_arch
    case "$(uname -m)" in
        x86_64)
            arch="x86_64"
            deb_arch="amd64"
            ;;
        aarch64|arm64)
            arch="arm64"
            deb_arch="arm64"
            ;;
        *)
            arch="$(uname -m)"
            deb_arch="$(uname -m)"
            ;;
    esac

    local tarball_name="${APP_NAME}-${VERSION}-linux-${arch}"
    if [ -f "$DIST_DIR/${tarball_name}.tar.gz" ]; then
        log_info "Tarball: $DIST_DIR/${tarball_name}.tar.gz"
        log_info "Size: $(du -h "$DIST_DIR/${tarball_name}.tar.gz" | cut -f1)"
    fi

    if $CREATE_APPIMAGE && [ -f "$DIST_DIR/${APP_NAME}-${VERSION}-linux-${arch}.AppImage" ]; then
        log_info "AppImage: $DIST_DIR/${APP_NAME}-${VERSION}-linux-${arch}.AppImage"
    fi

    local deb_file="$DIST_DIR/sotf-systemwide_${VERSION}_${deb_arch}.deb"
    if $CREATE_DEB && [ -f "$deb_file" ]; then
        log_info ".deb: $deb_file"
        log_info "Size: $(du -h "$deb_file" | cut -f1)"
        log_info ""
        log_info "Install with: sudo dpkg -i $deb_file"
        log_info "Then enable: systemctl --user enable --now sotf-daemon.service"
    fi
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    main "$@"
fi
