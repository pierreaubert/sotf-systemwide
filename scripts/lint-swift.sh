#!/usr/bin/env bash
# Swift lint gate: the clippy analog for the Swift code.
#
# SwiftLint is not part of the toolchain and is not installed in CI, so the
# control is the compiler itself: every Swift target must build with
# warnings treated as errors. Covers the ConfigBar package (app + tests)
# and the HAL driver sources (driver + its test runner), which are not a
# SwiftPM package and are type-checked directly.
set -euo pipefail

cd "$(dirname "$0")/.."

echo "==> ConfigBar (app + tests), warnings as errors"
swift build --build-tests \
    --package-path swift/configbar \
    --scratch-path target/configbar-swiftpm \
    -Xswiftc -warnings-as-errors

hal_sources="swift/driver-hal/Sources"
module_cache="${TMPDIR:-/tmp}/sotf-clang-module-cache"
mkdir -p "$module_cache"
driver_files=(
    "$hal_sources/Timing.swift"
    "$hal_sources/RingBuffer.swift"
    "$hal_sources/SharedMemory.swift"
    "$hal_sources/Encryption.swift"
    "$hal_sources/RealtimeChaChaPoly.swift"
    "$hal_sources/SotFHALDriver.swift"
)

echo "==> HAL driver type-check, warnings as errors"
swiftc -warnings-as-errors \
    -module-cache-path "$module_cache" \
    -typecheck \
    -module-name SotFHAL \
    -import-objc-header "$hal_sources/BridgingHeader.h" \
    -framework CoreAudio \
    -framework CoreFoundation \
    -framework Foundation \
    "${driver_files[@]}"

echo "==> HAL driver tests type-check, warnings as errors"
swiftc -warnings-as-errors \
    -D SOTF_HAL_TESTING \
    -module-cache-path "$module_cache" \
    -typecheck \
    -module-name SotFHALTests \
    -import-objc-header "$hal_sources/BridgingHeader.h" \
    -framework CoreAudio \
    -framework CoreFoundation \
    -framework Foundation \
    "${driver_files[@]}" \
    "$hal_sources/Tests.swift" \
    swift/driver-hal/TestsRunner.swift

echo "Swift lint clean."
