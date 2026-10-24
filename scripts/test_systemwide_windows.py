#!/usr/bin/env python3
"""Run the native Windows lab component gate with isolated daemon state."""

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import uuid

ROOT = Path(__file__).resolve().parent.parent


def require_native_windows():
    if sys.platform != "win32":
        raise RuntimeError("This gate requires native Windows Python; cross-compilation is not execution.")
    import ctypes
    if hasattr(ctypes.WinDLL("ntdll"), "wine_get_version"):
        raise RuntimeError("Wine is not native Windows acceptance.")
    if sys.version_info < (3, 11):
        raise RuntimeError("Python 3.11 or newer is required.")


def sources():
    paths = {ROOT / name for name in ("Cargo.toml", "Cargo.lock", "crates/daemon/Cargo.toml",
        "scripts/test_systemwide_windows.py", "scripts/bench_cpal_capture.py",
        "scripts/bench_lab_latency.py", "scripts/tests/test_cpal_capture_bench.py",
        "scripts/tests/test_lab_latency_bench.py", "scripts/tests/test_windows_gate.py",
        "../sotf-daw/Cargo.toml", "../sotf-daw/Cargo.lock")}
    for relative in ("crates/daemon", "crates/callback-metrics", "../sotf-daw/crates/driver-common",
                     "../sotf-daw/crates/sotf-engine", "../sotf-daw/crates/sotf-plugins"):
        paths.update(path for path in (ROOT / relative).rglob("*.rs") if "target" not in path.parts)
    return {os.path.relpath(path, ROOT): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted(paths)}


def steps(output):
    daw = ["--manifest-path", str(ROOT / "../sotf-daw/Cargo.toml")]
    plan = [("driver-common", ["cargo", "test", "--locked", *daw, "-p", "driver-common"])]
    for selector in ("lab_tests", "lab_pcm::tests",
                     "buffered_explicit_capture_is_drained_without_synthetic_frame_pacing"):
        plan.append(("engine-" + selector.replace("::", "-"),
            ["cargo", "test", "--locked", *daw, "-p", "sotf-engine", "--no-default-features",
             "--features", "test-util", "--lib", selector]))
    plan += [
        ("daemon-native-acl-and-ipc", ["cargo", "test", "--locked", "-p", "sotf-daemon"]),
        ("allocation-observer", ["cargo", "test", "--locked", "-p", "sotf-callback-metrics", "allocation::tests"]),
        ("capture-verifier", [sys.executable, "-m", "unittest", "discover", "-s", "scripts/tests",
                              "-p", "test_cpal_capture_bench.py", "-v"]),
        ("latency-provenance", [sys.executable, "-m", "unittest", "discover", "-s", "scripts/tests",
                                "-p", "test_lab_latency_bench.py", "-v"]),
        ("gate-control-flow", [sys.executable, "-m", "unittest", "discover", "-s", "scripts/tests",
                              "-p", "test_windows_gate.py", "-v"]),
        ("capture-closure-release", [sys.executable, str(ROOT / "scripts/bench_cpal_capture.py"),
                                     "--output-dir", str(output / "capture-closure")]),
        ("lab-latency-release", [sys.executable, str(ROOT / "scripts/bench_lab_latency.py"),
                                 "--output-dir", str(output / "lab-latency")]),
        ("driver-lint", ["cargo", "clippy", "--locked", *daw, "-p", "driver-common",
                         "--all-targets", "--no-deps", "--", "-D", "warnings"]),
        ("daemon-lint", ["cargo", "clippy", "--locked", "-p", "sotf-daemon", "-p", "sotf-callback-metrics",
                         "--all-targets", "--no-deps", "--", "-D", "warnings"]),
        ("latency-lint", ["cargo", "clippy", "--locked", "-p", "sotf-daemon", "--bench", "lab_latency_benchmark",
                          "--features", "lab-latency-bench", "--no-deps", "--", "-D", "warnings"]),
    ]
    return plan


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path)
    args = parser.parse_args()
    # Refuse before touching output paths or starting any process.
    require_native_windows()
    target = Path(os.environ.get("CARGO_TARGET_DIR", str(ROOT / "target")))
    if not target.is_absolute():
        target = ROOT / target
    output = (args.output_dir or target / "systemwide-windows-gate").resolve()
    output.mkdir(parents=True, exist_ok=True)
    final = output / "report.json"
    before_path, after_path = output / "sources-before.json", output / "sources-after.json"
    for path in (final, before_path, after_path):
        path.unlink(missing_ok=True)
    # The daemon creates this directory with its Windows security descriptor;
    # precreating it here would install Python's inherited/default owner first.
    runtime = output / f"runtime-{uuid.uuid4()}"
    environment = dict(os.environ, CARGO_TARGET_DIR=str(target.resolve()),
                       SOTF_SYSTEMWIDE_DRIVER="lab", SOTF_SYSTEMWIDE_RUNTIME_DIR=str(runtime),
                       SOTF_CPAL_METRICS_DIR=str(output / "capture-closure"))
    report = {"component_gate_passed": False, "audit_complete": False,
        "scope": "Native Windows lab daemon, single-account ACL/IPC, source-to-simulated-sink and closure measurements",
        "host": platform.platform(), "runtime_directory": str(runtime), "steps": [],
        "remaining_acceptance": ["two-account Windows ACL and credential isolation",
            "native capture/output device and notification/error storms", "physical loopback and deadline qualification"]}
    try:
        before = sources()
        before_path.write_text(json.dumps(before, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        for index, (name, command) in enumerate(steps(output), 1):
            print(f"==> {name}", flush=True)
            log = output / f"{index:02d}-{name}.log"
            with log.open("w", encoding="utf-8") as stream:
                process = subprocess.run(command, cwd=ROOT, env=environment, stdout=stream,
                                         stderr=subprocess.STDOUT, check=False)
            report["steps"].append({"name": name, "command": command, "exit_code": process.returncode,
                                    "log": str(log), "log_sha256": hashlib.sha256(log.read_bytes()).hexdigest()})
            if process.returncode != 0:
                print(f"Failed {name}; see {log}", file=sys.stderr)
                return 1
        report["measurement_reports"] = {}
        for name in ("capture-closure", "lab-latency"):
            path = output / name / "report.json"
            measurement = json.loads(path.read_text(encoding="utf-8"))
            snapshots = measurement["source_snapshot_sha256"]
            if snapshots["before"] != snapshots["after"]:
                raise ValueError(f"mismatched {name} source snapshots")
            for moment in ("before", "after"):
                snapshot = path.parent / f"sources-{moment}.json"
                if hashlib.sha256(snapshot.read_bytes()).hexdigest() != snapshots[moment]:
                    raise ValueError(f"invalid {name} snapshot hash")
            report["measurement_reports"][name] = {
                "path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                "case_count": len(measurement["cases"]), "binary_sha256": measurement["binary_sha256"]}
        after = sources()
        after_path.write_text(json.dumps(after, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        if before != after:
            report["source_changes"] = sorted(path for path in before.keys() | after.keys()
                                               if before.get(path) != after.get(path))
            print("Sources changed during the gate; component qualification withheld", file=sys.stderr)
            return 1
        report["selected_source_sha256"] = after
        report["component_gate_passed"] = True
        print("Windows lab component gate passed; two-account and physical/backend acceptance remain unverified.")
        return 0
    except Exception as error:
        report["error"] = str(error)
        print(f"Windows component gate failed: {error}", file=sys.stderr)
        return 1
    finally:
        report["finished_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        temporary = output / "report.json.tmp"
        temporary.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        temporary.replace(final)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except RuntimeError as error:
        print(str(error), file=sys.stderr)
        sys.exit(2)
