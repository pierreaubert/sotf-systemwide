# ----------------------------------------------------------------------
# RELEASE BUILDS (mirror of ../sotf/builds/cross.just, adapted)
#   - Linux + Windows are produced via Docker (scripts/cross-docker.sh).
#   - macOS is built natively on the host (cross-macos-* recipes).
#
# Umbrella recipes orchestrate both:
#   On an ARM Mac:      just cross-arm  (docker linux/windows arm64 + macos-arm64 pkg)
#   On an x86 machine:  just cross-x86  (docker linux/windows x86)
#
# The two-machine release (local ARM + spin.local x86, artifacts collected
# in dist/ with checksums) is `just release-local`, which drives
# scripts/build-release-local.sh with ~/.sotf-release.conf.
# ----------------------------------------------------------------------

# Build all ARM binaries (Linux + Windows in Docker, macOS pkg native)
[group('cross')]
[macos]
cross-arm: docker-linux-arm64 docker-windows-arm64 cross-macos-arm64

# Build all x86 binaries (Linux + Windows in Docker)
[group('cross')]
cross-x86: docker-linux-x86 docker-windows-x86

# Default: build for the current architecture
[group('cross')]
cross:
	#!/usr/bin/env bash
	set -euo pipefail
	arch=$(uname -m)
	case "$arch" in
		aarch64|arm64) just cross-arm ;;
		x86_64)        just cross-x86 ;;
		*) echo "Unsupported architecture: $arch"; exit 1 ;;
	esac

# ----------------------------------------------------------------------
# DOCKER TARGETS (thin wrappers over scripts/cross-docker.sh so the
# remote x86 builder needs no just installation)
# ----------------------------------------------------------------------

# Linux ARM64: AppImage + tarball + .deb -> dist/
[group('docker')]
docker-linux-arm64:
	./scripts/cross-docker.sh linux-arm64

# Linux x86_64: AppImage + tarball + .deb -> dist/
[group('docker')]
docker-linux-x86:
	./scripts/cross-docker.sh linux-x86

# Windows ARM64: sotf-daemon.exe zip -> dist/
[group('docker')]
docker-windows-arm64:
	./scripts/cross-docker.sh windows-arm64

# Windows x86_64: sotf-daemon.exe zip -> dist/
[group('docker')]
docker-windows-x86:
	./scripts/cross-docker.sh windows-x86

# ----------------------------------------------------------------------
# MACOS (native build, outputs to dist/)
# ----------------------------------------------------------------------

# macOS ARM64 installer pkg (daemon + ConfigBar + HAL driver) -> dist/
[group('cross')]
[macos]
cross-macos-arm64:
	./scripts/build-systemwide.sh

# ----------------------------------------------------------------------
# FULL RELEASE ORCHESTRATION
# ----------------------------------------------------------------------

# Build the full release matrix across this Mac + the configured x86
# builder and collect everything in dist/ (see scripts/build-release-local.sh --help)
[group('release')]
[macos]
release-local *args:
	./scripts/build-release-local.sh {{args}}

# Fast, Docker-free checks of the release scripts (syntax, zip packaging,
# orchestrator dry-run matrix)
[group('test')]
test-release-scripts:
	bash scripts/tests/test_build_release_scripts.sh
