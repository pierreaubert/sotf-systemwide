# --------------------------------------------------------- -*- just -*-
# sotf-systemwide: OS audio capture, plugin-chain processing, hardware output.
#
# SOTF crates are taken from the sibling `sotf` (sotf-player) and `sotf-daw`
# (sotf-engine, sotf-plugins, drivers) checkouts: a standalone clone of this
# repo needs both next to it (the all_of_sotf layout).
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
	cargo test --manifest-path ../sotf-daw/Cargo.toml -p driver-hal --lib
	cargo test -p sotf-daemon --test hal_driver_contract_tests
	swift test --package-path swift/configbar --scratch-path target/configbar-swiftpm

# ----------------------------------------------------------------------
# LINT
# ----------------------------------------------------------------------

[group('lint')]
lint:
	cargo clippy --workspace --all-targets --no-deps -- -- -D warnings

# ----------------------------------------------------------------------
# QA (same target name as sotf: lint + tests + the isolated macOS lab)
# ----------------------------------------------------------------------

# The lab never installs or touches the machine-wide CoreAudio HAL bundle
# (see systemwide-lab above); other platforms run lint + tests only.
[group('qa')]
[macos]
qa: lint test systemwide-lab

[group('qa')]
[linux]
qa: lint test

[group('qa')]
[windows]
qa: lint test

# ----------------------------------------------------------------------
# COVERAGE (same target names as sotf)
# ----------------------------------------------------------------------

# Requires: cargo install cargo-llvm-cov
[group('coverage')]
coverage:
	cargo llvm-cov --workspace --lib --bins --tests --examples --lcov --output-path target/lcov.info

# Generates an HTML coverage report and opens it.
[group('coverage')]
coverage-html:
	cargo llvm-cov --workspace --lib --bins --tests --examples --html --open

# Prints a text summary to stdout (fastest coverage recipe).
[group('coverage')]
coverage-summary:
	cargo llvm-cov --workspace --lib --bins --tests --examples --text --summary-only

# Removes stale coverage artifacts.
[group('coverage')]
coverage-clean:
	cargo llvm-cov clean

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
