#!/usr/bin/env bash

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: ./scripts/test-systemwide.sh <macos|linux>

Runs the native systemwide contract/component gate for the selected platform.
Use the just recipes instead of invoking the Linux mode directly on a host.
EOF
}

phase() {
    printf '\n==> %s\n' "$1"
}

require_platform() {
    local expected_os=$1
    local actual_os
    actual_os=$(uname -s)
    if [[ "$actual_os" != "$expected_os" ]]; then
        printf 'error: this gate requires %s, found %s\n' "$expected_os" "$actual_os" >&2
        exit 1
    fi
}

require_arm64() {
    local actual_arch
    actual_arch=$(uname -m)
    case "$actual_arch" in
        arm64|aarch64)
            ;;
        *)
            printf 'error: this gate requires ARM64, found %s\n' "$actual_arch" >&2
            exit 1
            ;;
    esac
}

test_rust_common() {
    phase "Shared lab clock, observation and capture pacing regressions"
    local engine_features="test-util"
    if [[ "$(uname -s)" = "Darwin" ]]; then
        engine_features="$engine_features,hal"
    fi
    local engine_filter
    for engine_filter in lab_tests lab_pcm::tests buffered_explicit_capture_is_drained_without_synthetic_frame_pacing; do
        cargo test --locked --manifest-path ../sotf-daw/Cargo.toml -p sotf-engine \
            --no-default-features --features "$engine_features" --lib "$engine_filter"
    done

    phase "driver-common tests"
    cargo test --locked --manifest-path ../sotf-daw/Cargo.toml -p driver-common

    phase "sotf-daemon tests (including real local IPC)"
    if [[ "$(uname -s)" = "Darwin" ]]; then
        cargo test --locked -p sotf-daemon --features hal
    else
        cargo test --locked -p sotf-daemon
    fi

    phase "Capture allocation observer self-tests and strict lint"
    cargo test --locked -p sotf-callback-metrics allocation::tests
    cargo clippy --locked -p sotf-callback-metrics --all-targets --no-deps -- -D warnings

    phase "Release CPAL capture closure allocation and timing"
    python3 -m unittest discover -s scripts/tests -p test_cpal_capture_bench.py -v
    bash scripts/bench-cpal-capture.sh

    phase "IPC benchmark protocol and timing canaries"
    python3 -m unittest discover -s scripts/tests -p test_ipc_control_bench.py -v

    phase "Release IPC control latency under concurrent load"
    bash scripts/bench-ipc-control.sh

    phase "Release source-to-sink latency and PCM integrity"
    python3 -m unittest discover -s scripts/tests -p test_lab_latency_bench.py -v
    bash scripts/bench-lab-latency.sh
    cargo clippy --locked -p sotf-daemon --bench lab_latency_benchmark \
        --features lab-latency-bench --no-deps -- -D warnings
}

test_macos() {
    require_platform Darwin

    phase "macOS HAL Rust tests"
    cargo test --locked --manifest-path ../sotf-daw/Cargo.toml -p driver-hal

    test_rust_common

    phase "strict Rust lint"
    cargo clippy --locked \
        --manifest-path ../sotf-daw/Cargo.toml \
        -p driver-common \
        -p driver-hal \
        --all-targets \
        --no-deps \
        -- \
        -D warnings
    cargo clippy --locked \
        -p sotf-daemon \
        --all-targets \
        --no-deps \
        --features sotf-daemon/hal \
        -- \
        -D warnings

    local configbar_dir="swift/configbar"
    local module_cache="${TMPDIR:-/tmp}/sotf-clang-module-cache"
    mkdir -p "$module_cache"
    local swift_build_dir="${CARGO_TARGET_DIR:-target}/systemwide-swift-package"
    mkdir -p "$swift_build_dir"

    phase "Swift menu-bar client type-check"
    swift build \
        --package-path "$configbar_dir" \
        --scratch-path "$swift_build_dir" \
        --target SotFSystemwide

    phase "ConfigBar XCTest suites"
    scripts/test-configbar.sh "$swift_build_dir"

    phase "Swift HAL behavioral tests"
    # The expanded sequence workload uses the shipping Rust worker. Keep the
    # debug worker explicitly selectable for separate diagnostic runs.
    SOTF_HAL_WORKER_PROFILE="${SOTF_HAL_WORKER_PROFILE:-release}" just test-hal-driver-swift

    phase "Release HAL callback allocations and CPU/wall distributions"
    # The malloc observer and sanitizer interposers need separate processes.
    SOTF_HAL_SANITIZER= just bench-hal-transport

    phase "Script behavior, installer privilege and packaging fixtures"
    "${PYTHON:-python3}" -m unittest discover -s scripts/tests -v

    local hal_sources="swift/driver-hal/Sources"
    phase "Swift HAL driver type-check"
    swiftc \
        -module-cache-path "$module_cache" \
        -typecheck \
        -module-name SotFHAL \
        -import-objc-header "$hal_sources/BridgingHeader.h" \
        -framework CoreAudio \
        -framework CoreFoundation \
        -framework Foundation \
        "$hal_sources/Timing.swift" \
        "$hal_sources/RingBuffer.swift" \
        "$hal_sources/SharedMemory.swift" \
        "$hal_sources/Encryption.swift" \
		"$hal_sources/RealtimeChaChaPoly.swift" \
        "$hal_sources/SotFHALDriver.swift"
}

test_linux() {
    require_platform Linux
    require_arm64

    phase "portable HAL streaming regression tests"
    cargo test --locked -p sotf-daemon --test hal_driver_contract_tests

    test_rust_common

    phase "strict Rust lint"
    cargo clippy --locked \
        --manifest-path ../sotf-daw/Cargo.toml \
        -p driver-common \
        -p driver-hal \
        --all-targets \
        --no-deps \
        -- \
        -D warnings
    cargo clippy --locked \
        -p sotf-daemon \
        --all-targets \
        --no-deps \
        -- \
        -D warnings
}

if [[ $# -ne 1 ]]; then
    usage >&2
    exit 2
fi

case "$1" in
    macos)
        test_macos
        ;;
    linux)
        test_linux
        ;;
    -h|--help)
        usage
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac
