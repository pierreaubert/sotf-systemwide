#!/usr/bin/env bash
set -euo pipefail

# Local release hot-path evidence. This does not measure hardware/DSP latency.
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
METRICS_ROOT="${SOTF_HAL_METRICS_DIR:-$ROOT/target/hal-transport-metrics}"
mkdir -p "$METRICS_ROOT/idle" "$METRICS_ROOT/control-storm"
# Remove only reports owned by this recipe, preventing stale-case reuse.
rm -f "$METRICS_ROOT/report.json"
for mode in idle control-storm; do
    rm -f "$METRICS_ROOT/$mode/swift-hal-callbacks.json"
    for side in rust-reader swift-writer; do
        rm -f "$METRICS_ROOT/$mode/$side-"*.json
    done
done
SOTF_HAL_ALLOCATION_PROBE=1 SOTF_HAL_WORKER_PROFILE=release SOTF_HAL_METRICS_DIR="$METRICS_ROOT/idle" \
    just test-hal-driver-swift
# Reuse the executable just built so both load cases use identical code.
WORKER_ROOT="${CARGO_TARGET_DIR:-target}"
if [[ "$WORKER_ROOT" != /* ]]; then WORKER_ROOT="$ROOT/$WORKER_ROOT"; fi
TEST_BINARY="$ROOT/target/debug/SotFHALAllocationTests.bundle/Contents/MacOS/SotFHALTests"
SOTF_HAL_MEASUREMENT_STORM=1 SOTF_HAL_METRICS_DIR="$METRICS_ROOT/control-storm" \
    SOTF_RUST_HAL_TRANSPORT_WORKER="$WORKER_ROOT/release/hal-transport-worker" \
    SOTF_RUST_HAL_CAPTURE_WORKER="$WORKER_ROOT/release/hal-capture-stress" \
    SOTF_SWIFT_HAL_ATTACHMENT_PROBE="$(dirname "$TEST_BINARY")/SotFHALAttachmentProbe" \
    "$TEST_BINARY"
python3 - "$METRICS_ROOT" <<'PY'
import datetime
import hashlib
import json
import subprocess
import pathlib
import platform
import sys

root = pathlib.Path(sys.argv[1])
cases = []
callbacks = []
for mode in ("idle", "control-storm"):
    files = sorted((root / mode).glob("rust-reader-*.json")) + sorted((root / mode).glob("swift-writer-*.json"))
    expected = {
        f"{side}-{channels}ch-{rate}hz-{frames}frames-{encryption}"
        for side in ("rust-reader", "swift-writer")
        for channels in (2, 8, 32)
        for rate in (48000, 96000, 192000)
        for frames in (64, 128, 512)
        for encryption in ("plaintext", "encrypted")
    }
    if {path.stem for path in files} != expected:
        raise SystemExit(f"incomplete geometry matrix for {mode}: expected {len(expected)} reports, got {len(files)}")
    for path in files:
        record = json.loads(path.read_text())
        if record["case"] != path.stem:
            raise SystemExit(f"case identity mismatch in {path}")
        side = "rust-reader" if path.stem.startswith("rust-reader-") else "swift-writer"
        encryption = "encrypted" if path.stem.endswith("-encrypted") else "plaintext"
        identity = f'{side}-{record["channels"]}ch-{record["sample_rate"]}hz-{record["buffer_frames"]}frames-{encryption}'
        if identity != path.stem:
            raise SystemExit(f"geometry identity mismatch in {path}")
        ring_frames = record["buffer_frames"] * 8 * ((record["sample_rate"] + 47999) // 48000)
        if record["verified_frames"] != ring_frames * 32 or record["ring_lengths"] != 32:
            raise SystemExit(f"incomplete sample sequence in {path}")
        if path.stem.startswith("rust-reader-"):
            if record["allocation_calls"] != 0:
                raise SystemExit(f"audio reader allocated in {path}")
            if mode == "control-storm" and record["control_storm_updates"] <= 0:
                raise SystemExit(f"inactive control storm in {path}")
        elif record["rejected_writes"] <= 0:
            raise SystemExit(f"overflow was not exercised in {path}")
        record["load"] = mode
        cases.append(record)
    callback_report = json.loads((root / mode / "swift-hal-callbacks.json").read_text())
    if len(callback_report["cases"]) != 576:
        raise SystemExit("expected all 576 HAL callback cases")
    if callback_report["observer_canary_allocations"] <= 0 or callback_report["observer_canary_deallocations"] <= 0:
        raise SystemExit("inactive Swift allocation observer")
    for record in callback_report["cases"]:
        record["load"] = mode
        callbacks.append(record)
sources = ["swift/driver-hal/Sources/SharedMemory.swift", "swift/driver-hal/Sources/Tests.swift",
           "swift/driver-hal/Sources/SotFHALDriver.swift", "swift/driver-hal/TestAllocationObserver.c",
           "../sotf-daw/crates/driver-hal/src/shared_memory/shared_audio_buffer.rs",
           "../sotf-daw/crates/driver-hal/src/shared_memory/hal_input_reader.rs",
           "../sotf-daw/crates/driver-hal/src/bin/hal_capture_stress.rs",
           "../sotf-daw/crates/driver-hal/src/bin/hal_capture_stress/metrics.rs"]
report = {"created_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
          "source_sha256": {p: hashlib.sha256(pathlib.Path(p).read_bytes()).hexdigest() for p in sources},
          "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
          "swift": subprocess.check_output(["swift", "--version"], text=True).strip(),
          "host": platform.platform(), "rust_profile": "release", "swift_optimization": "-O",
          "limits": "Host fixtures; no installed HAL, hardware, CPAL/DSP callbacks or end-to-end latency. Swift counts use a malloc-family interposer.",
          "cases": cases, "hal_callback_cases": callbacks}
(root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
print(f"Wrote {root / 'report.json'}")
PY
