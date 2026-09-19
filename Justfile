# --------------------------------------------------------- -*- just -*-
# sotf-systemwide: OS audio capture, plugin-chain processing, hardware output.
#
# SOTF crates (sotf-engine, sotf-player, sotf-plugins) are taken from the
# sibling `sotf` checkout: a standalone clone of this repo needs `../sotf`
# next to it (the all_of_sotf layout).
# ----------------------------------------------------------------------

_default:
	just --list

import 'builds/macos.just'
import 'builds/systemwide.just'

# ----------------------------------------------------------------------
# TEST
# ----------------------------------------------------------------------

[group('test')]
check:
	cargo check --workspace --lib --bins --tests --examples

[group('test')]
test:
	cargo test --workspace --lib --bins --tests --examples

# Run the isolated macOS systemwide-audio lab. This does not install or touch
# the CoreAudio HAL bundle; subprocess tests use temporary Unix sockets and
# the deterministic lab driver.
[group('test')]
[macos]
systemwide-lab:
	#!/usr/bin/env bash
	set -euo pipefail
	runtime_dir="$(mktemp -d /private/tmp/sotf-systemwide-lab.XXXXXX)"
	trap 'rm -rf "$runtime_dir"' EXIT
	export SOTF_SYSTEMWIDE_RUNTIME_DIR="$runtime_dir"
	cargo test -p sotf-daemon --bin sotf-daemon testkit
	cargo test -p sotf-daemon --test daemon_state_tests
	cargo test -p sotf-daemon --features hal --test ipc_line_tests -- --test-threads=1
	cargo test -p driver-hal --lib
	cargo test -p driver-hal --test streaming_regression_tests
	swift test --package-path swift/configbar --scratch-path target/configbar-swiftpm

# ----------------------------------------------------------------------
# LINT
# ----------------------------------------------------------------------

[group('lint')]
lint:
	cargo clippy --workspace --all-targets --no-deps -- -- -D warnings

# ----------------------------------------------------------------------
# FORMAT
# ----------------------------------------------------------------------

alias format := fmt

fmt:
	cargo fmt --all

# ----------------------------------------------------------------------
# CLEAN
# ----------------------------------------------------------------------

clean:
	cargo clean
	find . -name '*~' -exec rm {} \; -print
