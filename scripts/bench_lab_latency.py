#!/usr/bin/env python3
"""Build, validate and record the explicitly selected lab latency benchmark."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import time

try:
    import resource
except ImportError:
    resource = None


ROOT = Path(__file__).resolve().parent.parent


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def sources():
    paths = {
        ROOT / "Cargo.toml", ROOT / "Cargo.lock", ROOT / "crates/daemon/Cargo.toml",
        ROOT / "crates/daemon/benches/lab_latency_benchmark.rs", Path(__file__).resolve(),
        ROOT / "scripts/bench-lab-latency.sh", ROOT / "builds/systemwide.just",
        ROOT / "../sotf-daw/Cargo.toml",
    }
    for relative in [
        "../sotf-daw/crates/driver-common", "../sotf-daw/crates/driver-hal", "../sotf-daw/crates/sotf-engine",
        "../sotf-daw/crates/sotf-plugins/src", "../sotf-daw/crates/sotf-plugins/crates/sotf-host",
        "../sotf-daw/crates/sotf-plugins/crates/sotf-plugin-gain",
        "../sotf-daw/crates/sotf-plugins/crates/sotf-plugin-matrix",
    ]:
        paths.update(path for path in (ROOT / relative).rglob("*.rs") if "target" not in path.parts)
        manifest = ROOT / relative / "Cargo.toml"
        if manifest.exists():
            paths.add(manifest)
    return {os.path.relpath(path, ROOT): digest(path) for path in sorted(paths)}


def distribution(values):
    values = sorted(values)
    if not values:
        raise ValueError("empty latency observation")
    result = {f"p{p}_ns": values[(len(values) * p + 99) // 100 - 1] for p in [50, 95, 99]}
    result.update(count=len(values), max_ns=values[-1])
    return result


def validate(report, smoke):
    expected = {(2, 48000, 128, False), (32, 96000, 512, True)} if smoke else {
        (channels, rate, block, half)
        for channels in [2, 8, 32] for rate in [48000, 96000, 192000]
        for block in [64, 128, 512] for half in [False, True]
    }
    seen = set()
    if not report.get("complete") or report.get("smoke") != smoke:
        raise ValueError("incomplete or mismatched benchmark mode")
    for case in report["cases"]:
        shape = (case["input_channels"], case["sample_rate"], case["block_frames"], case["gain_db"] < 0)
        if shape not in expected or shape in seen:
            raise ValueError(f"unexpected/duplicate geometry {shape}")
        seen.add(shape)
        channels, rate, block, _ = shape
        frames = ((rate + block - 1) // block) * block
        observations = case["observations"]
        if (case["source_frames_consumed"] != case["warmup_frames"] + frames
                or case["output_channels"] != min(channels, 16)
                or observations["marked_frames"] != frames
                or observations["verified_samples"] != frames * case["output_channels"]
                or observations["invalid_samples"] != 0
                or observations["first_error"] is not None
                or observations["first_generation"] != observations["last_generation"]
                or observations["first_generation"] is None):
            raise ValueError(f"invalid PCM/generation proof for {shape}")
        pairs = observations["timing_pairs"]
        if [pair["first_marker_frame"] for pair in pairs] != list(range(0, frames, block)):
            raise ValueError(f"missing/duplicated timestamp pairs for {shape}")
        previous_sink = 0
        for pair in pairs:
            expected_capture = (case["warmup_frames"] + pair["first_marker_frame"]) * 1000000000 // rate
            if (pair["virtual_capture_ns"] != expected_capture or pair["source_read_ns"] <= 0
                    or not expected_capture <= pair["source_read_ns"] <= pair["sink_observed_ns"]
                    or pair["sink_observed_ns"] < previous_sink):
                raise ValueError(f"invalid monotonic timing pair for {shape}")
            previous_sink = pair["sink_observed_ns"]
        for label, origin in [("virtual_capture_to_sink", "virtual_capture_ns"),
                              ("source_read_to_sink", "source_read_ns")]:
            recalculated = distribution([pair["sink_observed_ns"] - pair[origin] for pair in pairs])
            if case[label] != recalculated:
                raise ValueError(f"incorrect {label} quantiles for {shape}")
    if seen != expected:
        raise ValueError("missing geometry/profile cases")


def cpu_usage():
    if resource is None:
        return None
    value = resource.getrusage(resource.RUSAGE_CHILDREN)
    return value.ru_utime, value.ru_stime


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--smoke", action="store_true")
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--no-hal", action="store_true", help="exercise the portable build even on macOS")
    args = parser.parse_args()
    target = Path(os.environ.get("CARGO_TARGET_DIR", str(ROOT / "target")))
    if not target.is_absolute():
        target = ROOT / target
    output = args.output_dir or target / "lab-latency"
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    final = output / "report.json"
    raw = output / "raw.json"
    final.unlink(missing_ok=True)
    raw.unlink(missing_ok=True)
    before_path = output / "sources-before.json"
    after_path = output / "sources-after.json"
    before_path.unlink(missing_ok=True)
    after_path.unlink(missing_ok=True)
    before = sources()
    before_path.write_text(json.dumps(before, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    features = "lab-latency-bench"
    if platform.system() == "Darwin" and not args.no_hal:
        features += ",hal"
    build_log = output / "build.jsonl"
    with build_log.open("w", encoding="utf-8") as stream:
        subprocess.run(["cargo", "bench", "-p", "sotf-daemon", "--features", features,
                        "--bench", "lab_latency_benchmark", "--no-run", "--message-format=json"],
                       cwd=ROOT, stdout=stream, check=True)
    executables = set()
    for line in build_log.read_text(encoding="utf-8").splitlines():
        record = json.loads(line)
        if (record.get("reason") == "compiler-artifact"
                and record["target"]["name"] == "lab_latency_benchmark" and record.get("executable")):
            executables.add(record["executable"])
    if len(executables) != 1:
        raise ValueError(f"expected one benchmark executable, got {executables}")
    executable = Path(executables.pop())
    environment = dict(os.environ, SOTF_LAB_LATENCY_REPORT=str(raw))
    command = [str(executable)] + (["--smoke"] if args.smoke else [])
    cpu_before = cpu_usage()
    load_before = os.getloadavg() if hasattr(os, "getloadavg") else None
    start = time.monotonic()
    subprocess.run(command, cwd=ROOT, env=environment, check=True)
    elapsed = time.monotonic() - start
    cpu_after = cpu_usage()
    after = sources()
    after_path.write_text(json.dumps(after, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    if before != after:
        changed = sorted(path for path in before.keys() | after.keys() if before.get(path) != after.get(path))
        raise RuntimeError(f"benchmark sources changed during build/run: {', '.join(changed)}; no verified aggregate published")
    report = json.loads(raw.read_text(encoding="utf-8"))
    validate(report, args.smoke)
    report.update(source_sha256=after, binary_sha256=digest(executable), executable=str(executable),
                  host=platform.platform(), cpu_count=os.cpu_count(), load_average_before=load_before,
                  elapsed_seconds=elapsed, build_log_sha256=digest(build_log),
                  cargo_features=features,
                  source_snapshot_sha256={"before": digest(before_path), "after": digest(after_path)},
                  rustc=subprocess.check_output(["rustc", "--version"], text=True).strip())
    if cpu_before and cpu_after:
        report["benchmark_child_cpu_seconds"] = {
            "user": cpu_after[0] - cpu_before[0], "system": cpu_after[1] - cpu_before[1],
            "scope": "all benchmark process threads including observer, excludes cargo build",
        }
    temporary = output / "report.json.tmp"
    temporary.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    temporary.replace(final)
    silence = sum(case["observations"]["silent_frames_after_signal"] for case in report["cases"])
    print(f"Validated {len(report['cases'])} cases; {silence} silent frames after signal; wrote {final}")


if __name__ == "__main__":
    main()
