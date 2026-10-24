#!/usr/bin/env python3
"""Measure the production capture closure without opening audio devices."""

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parent.parent
FORMATS = ("f32", "f64", "i8", "i16", "i24", "i32", "i64", "u8", "u16", "u24", "u32", "u64")


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def sources():
    paths = ["Cargo.toml", "Cargo.lock", "crates/daemon/bin/cpal_capture.rs",
             "crates/callback-metrics/Cargo.toml", "crates/callback-metrics/src/main.rs",
             "crates/callback-metrics/src/allocation.rs", "crates/callback-metrics/src/clock.rs",
             "scripts/bench-cpal-capture.sh", "scripts/bench_cpal_capture.py"]
    return {path: digest(ROOT / path) for path in paths}


def validate(report):
    expected = {(sample_format, channels, frames, mode) for sample_format in FORMATS
                for channels in (2, 8, 32) for frames in (64, 512)
                for mode in ("drain", "overflow", "contended", "poisoned")}
    seen = set()
    for case in report["cases"]:
        shape = tuple(case[key] for key in ("format", "channels", "frames", "mode"))
        if shape not in expected or shape in seen:
            raise ValueError(f"unexpected or duplicate capture case: {shape}")
        seen.add(shape)
        counts = [case[key] for key in ("calls", "allocations", "deallocations",
                  "first_callback_allocations", "first_callback_deallocations",
                  "observer_canary_allocations", "observer_canary_deallocations")]
        if any(type(value) is not int or value < 0 for value in counts):
            raise ValueError(f"invalid capture counter: {shape}")
        if case["calls"] != 2000:
            raise ValueError(f"incomplete capture measurement: {shape}")
        if any(case[key] != 0 for key in ("allocations", "deallocations",
                "first_callback_allocations", "first_callback_deallocations")):
            raise ValueError(f"capture closure allocated or freed memory: {shape}")
        if case["observer_canary_allocations"] <= 0 or case["observer_canary_deallocations"] <= 0:
            raise ValueError(f"inactive allocation observer: {shape}")
        times = [case[key] for key in ("p50_ns", "p95_ns", "p99_ns", "max_ns")]
        if any(type(value) is not int or value < 0 for value in times) or times != sorted(times):
            raise ValueError(f"invalid capture timing distribution: {shape}")
    if seen != expected:
        raise ValueError("incomplete capture geometry/format/mode matrix")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path)
    args = parser.parse_args()
    default = Path(os.environ.get("CARGO_TARGET_DIR", "target")) / "cpal-callback-metrics"
    output = args.output_dir or Path(os.environ.get("SOTF_CPAL_METRICS_DIR", str(default)))
    if not output.is_absolute():
        output = ROOT / output
    output.mkdir(parents=True, exist_ok=True)
    final, raw = output / "report.json", output / "raw.json"
    before_path, after_path = output / "sources-before.json", output / "sources-after.json"
    for path in (final, raw, before_path, after_path):
        path.unlink(missing_ok=True)
    before = sources()
    before_path.write_text(json.dumps(before, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    build_log = output / "build.jsonl"
    with build_log.open("w", encoding="utf-8") as stream:
        subprocess.run(["cargo", "build", "--locked", "--release", "-p", "sotf-callback-metrics",
                        "--message-format=json"], cwd=ROOT, stdout=stream, check=True)
    executables = set()
    for line in build_log.read_text(encoding="utf-8").splitlines():
        record = json.loads(line)
        if (record.get("reason") == "compiler-artifact"
                and record["target"]["name"] == "sotf-callback-metrics" and record.get("executable")):
            executables.add(record["executable"])
    if len(executables) != 1:
        raise ValueError("expected one capture-metrics executable")
    executable = Path(executables.pop())
    subprocess.run([str(executable), str(raw)], cwd=ROOT, check=True)
    after = sources()
    after_path.write_text(json.dumps(after, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    if before != after:
        changed = sorted(path for path in before.keys() | after.keys() if before.get(path) != after.get(path))
        raise RuntimeError(f"capture benchmark sources changed: {', '.join(changed)}; no qualified report")
    report = json.loads(raw.read_text(encoding="utf-8"))
    validate(report)
    report.update(created_at=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  host=platform.platform(), profile="release", source_sha256=after,
                  rustc=subprocess.check_output(["rustc", "--version"], text=True).strip(),
                  binary_sha256=digest(executable), executable=str(executable),
                  build_log_sha256=digest(build_log),
                  source_snapshot_sha256={"before": digest(before_path), "after": digest(after_path)},
                  parking_lot_workspace_features=tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
                      ["workspace"]["dependencies"]["parking_lot"]["features"],
                  limits=["No device is opened; native CPAL backend/trampoline and DSP/output callbacks are excluded.",
                          "Each case uses a fresh Rust thread; observer canaries precede the first callback.",
                          "Counts cover Rust GlobalAlloc calls; foreign C allocator activity is not counted.",
                          "CPU/clock/observer overhead is included; no real-time deadline certification.",
                          "Control-thread contention and poisoned partial-frame fixtures are included."])
    temporary = output / "report.json.tmp"
    temporary.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    temporary.replace(final)
    print(f"Validated {len(report['cases'])} capture cases; wrote {final}")


if __name__ == "__main__":
    main()
