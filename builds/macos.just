# macos specific
# HDF5_DIR := '/opt/homebrew/Cellar/hdf5/2.0'

# should be done automatically
dyld_fallback_library_path := '/Applications/Xcode.app/Contents/Framework'

[group('build-macos')]
prod-hal:
	cargo build --release -p driver-hal

# Build the sotf-daemon binary (macOS with HAL support)
[group('build-macos')]
prod-daemon:
	cargo build --release -p sotf-daemon --features hal

# Build the SotF Systemwide Swift app
[group('build-macos')]
prod-systemwide:
	#!/usr/bin/env bash
	set -euo pipefail
	CONFIGBAR_DIR="swift/configbar"
	BUILD_DIR="target/release"
	mkdir -p "$BUILD_DIR"
	echo "Building SotF Systemwide..."
	SWIFT_BUILD_DIR="$BUILD_DIR/configbar-swiftpm"
	SWIFT_BIN_PATH="$(swift build --package-path "$CONFIGBAR_DIR" --scratch-path "$SWIFT_BUILD_DIR" --configuration release --show-bin-path)"
	swift build --package-path "$CONFIGBAR_DIR" --scratch-path "$SWIFT_BUILD_DIR" --configuration release --product sotf-systemwide
	cp "$SWIFT_BIN_PATH/sotf-systemwide" "$BUILD_DIR/sotf-systemwide"
	echo "✅ Systemwide built: $BUILD_DIR/sotf-systemwide"

# Backward-compatible build target.
[group('build-macos')]
prod-toolbar: prod-systemwide
	@echo "✅ prod-toolbar is an alias for prod-systemwide"

# Build the Swift HAL driver bundle
[group('build-macos')]
prod-hal-driver:
	#!/usr/bin/env bash
	set -euo pipefail
	HAL_SWIFT_DIR="swift/driver-hal"
	BUILD_DIR="target/release/SotFHAL.driver"
	mkdir -p "$BUILD_DIR/Contents/MacOS"
	mkdir -p "$BUILD_DIR/Contents/Resources"
	echo "Building Swift HAL driver..."
	swiftc \
		-emit-library \
		-o "$BUILD_DIR/Contents/MacOS/SotFHAL" \
		-module-name SotFHAL \
		-import-objc-header "$HAL_SWIFT_DIR/Sources/BridgingHeader.h" \
		-Xlinker -bundle \
		-Xlinker -rpath -Xlinker @loader_path/../Frameworks \
		-framework CoreAudio \
		-framework CoreFoundation \
		-framework Foundation \
		-O \
		"$HAL_SWIFT_DIR/Sources/Timing.swift" \
		"$HAL_SWIFT_DIR/Sources/RingBuffer.swift" \
		"$HAL_SWIFT_DIR/Sources/SharedMemory.swift" \
		"$HAL_SWIFT_DIR/Sources/Encryption.swift" \
		"$HAL_SWIFT_DIR/Sources/RealtimeChaChaPoly.swift" \
		"$HAL_SWIFT_DIR/Sources/SotFHALDriver.swift"
	cp "$HAL_SWIFT_DIR/Info.plist" "$BUILD_DIR/Contents/Info.plist"
	cp "crates/driver-hal/shared_memory_layout.json" \
		"$BUILD_DIR/Contents/Resources/shared_memory_layout.json"
	chmod 755 "$BUILD_DIR/Contents/MacOS/SotFHAL"
	chmod 644 "$BUILD_DIR/Contents/Info.plist"
	# Ad-hoc sign the driver bundle so CoreAudio will load it locally.
	# Distribution signing (Developer ID Application) happens at packaging time.
	echo "Ad-hoc signing HAL driver..."
	codesign --force --deep --sign - "$BUILD_DIR"
	echo "✅ HAL driver built: $BUILD_DIR"

# Run the assertion-based HAL tests as an executable bundle. This exercises
# the same Swift sources as the driver and verifies that the ABI manifest is
# discoverable from bundle resources, not only through a source-tree path.
[group('test')]
test-hal-driver-swift:
	#!/usr/bin/env bash
	set -euo pipefail
	CLANG_CACHE_DIR="${TMPDIR:-/tmp}/sotf-clang-module-cache"
	mkdir -p "$CLANG_CACHE_DIR"
	HAL_SWIFT_DIR="swift/driver-hal"
	BUILD_DIR="target/debug/SotFHALTests.bundle"
	mkdir -p "$BUILD_DIR/Contents/MacOS" "$BUILD_DIR/Contents/Resources"
	WORKER_TARGET_DIR="${CARGO_TARGET_DIR:-target}"
	cargo build -p driver-hal --features swift-stress-worker --bin hal-transport-worker
	if [[ "$WORKER_TARGET_DIR" = /* ]]; then
		WORKER_PATH="$WORKER_TARGET_DIR/debug/hal-transport-worker"
	else
		WORKER_PATH="$(pwd)/$WORKER_TARGET_DIR/debug/hal-transport-worker"
	fi
	test -x "$WORKER_PATH"
	echo "Building and running Swift HAL tests..."
	CLANG_MODULE_CACHE_PATH="$CLANG_CACHE_DIR" swiftc \
		-o "$BUILD_DIR/Contents/MacOS/SotFHALTests" \
		-module-name SotFHALTests \
		-import-objc-header "$HAL_SWIFT_DIR/Sources/BridgingHeader.h" \
		-framework CoreAudio \
		-framework CoreFoundation \
		-framework Foundation \
		-O \
		"$HAL_SWIFT_DIR/Sources/Timing.swift" \
		"$HAL_SWIFT_DIR/Sources/RingBuffer.swift" \
		"$HAL_SWIFT_DIR/Sources/SharedMemory.swift" \
		"$HAL_SWIFT_DIR/Sources/Encryption.swift" \
		"$HAL_SWIFT_DIR/Sources/RealtimeChaChaPoly.swift" \
		"$HAL_SWIFT_DIR/Sources/SotFHALDriver.swift" \
		"$HAL_SWIFT_DIR/Sources/Tests.swift" \
		"$HAL_SWIFT_DIR/TestsRunner.swift"
	cp "$HAL_SWIFT_DIR/TestsBundleInfo.plist" "$BUILD_DIR/Contents/Info.plist"
	cp "crates/driver-hal/shared_memory_layout.json" \
		"$BUILD_DIR/Contents/Resources/shared_memory_layout.json"
	chmod 755 "$BUILD_DIR/Contents/MacOS/SotFHALTests"
	SOTF_RUST_HAL_TRANSPORT_WORKER="$WORKER_PATH" \
		"$BUILD_DIR/Contents/MacOS/SotFHALTests"

# Build all macOS daemon components (daemon + Systemwide app + HAL driver)
[group('build-macos')]
prod-macos-daemon: prod-daemon prod-systemwide prod-hal-driver
	@echo "✅ All macOS daemon components built"

# ----------------------------------------------------------------------
# DIST variants (release-cut profile = fat LTO + codegen-units=1).
# Artifacts land in `target/dist/` instead of `target/release/`.
# Swift outputs follow the same convention so build-systemwide.sh can
# package every component from a single tree.
# ----------------------------------------------------------------------

[group('dist')]
dist-daemon:
	cargo build --profile dist -p sotf-daemon --features hal

[group('dist')]
dist-systemwide:
	#!/usr/bin/env bash
	set -euo pipefail
	CONFIGBAR_DIR="swift/configbar"
	BUILD_DIR="target/dist"
	mkdir -p "$BUILD_DIR"
	echo "Building SotF Systemwide (dist)..."
	SWIFT_BUILD_DIR="$BUILD_DIR/configbar-swiftpm"
	SWIFT_BIN_PATH="$(swift build --package-path "$CONFIGBAR_DIR" --scratch-path "$SWIFT_BUILD_DIR" --configuration release --show-bin-path)"
	swift build --package-path "$CONFIGBAR_DIR" --scratch-path "$SWIFT_BUILD_DIR" --configuration release --product sotf-systemwide
	cp "$SWIFT_BIN_PATH/sotf-systemwide" "$BUILD_DIR/sotf-systemwide"
	echo "✅ Systemwide built: $BUILD_DIR/sotf-systemwide"

[group('dist')]
dist-hal-driver:
	#!/usr/bin/env bash
	set -euo pipefail
	HAL_SWIFT_DIR="swift/driver-hal"
	BUILD_DIR="target/dist/SotFHAL.driver"
	mkdir -p "$BUILD_DIR/Contents/MacOS"
	mkdir -p "$BUILD_DIR/Contents/Resources"
	echo "Building Swift HAL driver (dist)..."
	swiftc \
		-emit-library \
		-o "$BUILD_DIR/Contents/MacOS/SotFHAL" \
		-module-name SotFHAL \
		-import-objc-header "$HAL_SWIFT_DIR/Sources/BridgingHeader.h" \
		-Xlinker -bundle \
		-Xlinker -rpath -Xlinker @loader_path/../Frameworks \
		-framework CoreAudio \
		-framework CoreFoundation \
		-framework Foundation \
		-O \
		"$HAL_SWIFT_DIR/Sources/Timing.swift" \
		"$HAL_SWIFT_DIR/Sources/RingBuffer.swift" \
		"$HAL_SWIFT_DIR/Sources/SharedMemory.swift" \
		"$HAL_SWIFT_DIR/Sources/Encryption.swift" \
		"$HAL_SWIFT_DIR/Sources/RealtimeChaChaPoly.swift" \
		"$HAL_SWIFT_DIR/Sources/SotFHALDriver.swift"
	cp "$HAL_SWIFT_DIR/Info.plist" "$BUILD_DIR/Contents/Info.plist"
	cp "crates/driver-hal/shared_memory_layout.json" \
		"$BUILD_DIR/Contents/Resources/shared_memory_layout.json"
	chmod 755 "$BUILD_DIR/Contents/MacOS/SotFHAL"
	chmod 644 "$BUILD_DIR/Contents/Info.plist"
	echo "Ad-hoc signing HAL driver..."
	codesign --force --deep --sign - "$BUILD_DIR"
	echo "✅ HAL driver built: $BUILD_DIR"

[group('dist')]
dist-macos-daemon: dist-daemon dist-systemwide dist-hal-driver
	@echo "✅ All macOS daemon components built (dist)"

# ----------------------------------------------------------------------
# MACOS INSTALLER
# ----------------------------------------------------------------------

# Build Systemwide package
[group('build-macos')]
build-systemwide:
	./scripts/build-systemwide.sh

# Backward-compatible alias for the old package target name.
[group('build-macos')]
build-daemon-dmg: build-systemwide
	@echo "✅ build-daemon-dmg is an alias for build-systemwide"

# Uninstall SotF systemwide components
[group('build-macos')]
uninstall-sotf:
	./scripts/uninstall-sotf.sh
