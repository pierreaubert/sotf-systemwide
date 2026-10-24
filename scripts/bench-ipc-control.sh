#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
METRICS_ROOT="${SOTF_IPC_METRICS_DIR:-${CARGO_TARGET_DIR:-target}/ipc-control-metrics}"
mkdir -p "$METRICS_ROOT"
features=()
if [[ "$(uname -s)" = "Darwin" ]]; then features=(--features hal); fi
cargo build --locked --release -p sotf-daemon "${features[@]}"
python3 scripts/bench_ipc_control.py \
    --binary "${CARGO_TARGET_DIR:-target}/release/sotf-daemon" \
    --output "$METRICS_ROOT/report.json" --profile release "$@"
