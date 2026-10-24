#!/usr/bin/env python3
"""Measure real lab-daemon IPC round trips using fresh Unix connections.

Run through bench-ipc-control.sh for a release build. Only the owned child uses
the temporary endpoint, state and session keys. No physical device is opened.
This measures client-observed control latency, including client scheduling and
JSON decoding; it does not measure audio callback or physical loopback latency.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime
import hashlib
import json
import math
import os
import pathlib
import platform
import select
import socket
import subprocess
import tempfile
import threading
import time

MAX_RESPONSE_BYTES = 2 * 1024 * 1024
PROBES = ("ping", "get_snapshot", "get_metering")


def request(endpoint: str, body: bytes, timeout: float) -> tuple[dict, int]:
    """Time socket creation through a complete, bounded, decoded response."""
    start = time.perf_counter_ns()
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(timeout)
        client.connect(endpoint)
        client.sendall(body + b"\n")
        data = bytearray()
        while not data.endswith(b"\n"):
            chunk = client.recv(min(65536, MAX_RESPONSE_BYTES + 1 - len(data)))
            if not chunk:
                raise RuntimeError("daemon closed before a complete response")
            data.extend(chunk)
            if len(data) > MAX_RESPONSE_BYTES:
                raise RuntimeError("response exceeded benchmark safety bound")
        response = json.loads(data)
        if not isinstance(response, dict) or not isinstance(response.get("success"), bool):
            raise RuntimeError("invalid daemon response envelope")
    return response, time.perf_counter_ns() - start


def encoded(command: dict) -> bytes:
    return json.dumps(command, separators=(",", ":")).encode()


def checked_request(endpoint: str, command: dict, timeout: float = 3.0) -> tuple[dict, int]:
    response, elapsed = request(endpoint, encoded(command), timeout)
    if not response["success"]:
        raise RuntimeError(f"{command['command']} failed: {response.get('error')}")
    return response, elapsed


def distribution(values: list[int]) -> dict:
    """Retain nearest-rank quantiles and every measured duration."""
    ordered = sorted(values)
    if not ordered:
        return {"count": 0, "durations_ns": []}
    result = {f"p{q}_ns": ordered[math.ceil(len(ordered) * q / 100) - 1] for q in (50, 95, 99)}
    return {"count": len(values), "min_ns": ordered[0], **result, "max_ns": ordered[-1],
            "durations_ns": values}


def selected_sources() -> dict[str, str]:
    paths = {pathlib.Path(__file__), pathlib.Path("scripts/bench-ipc-control.sh"),
             pathlib.Path("Cargo.toml"), pathlib.Path("Cargo.lock")}
    for directory in ("crates/daemon/bin", "../sotf-daw/crates/sotf-engine/src",
                      "../sotf-daw/crates/driver-common/src"):
        paths.update(pathlib.Path(directory).rglob("*.rs"))
    return {str(path): hashlib.sha256(path.read_bytes()).hexdigest() for path in sorted(paths)}


def probe(endpoint: str, command: str, stop: threading.Event, args) -> dict:
    durations: list[int] = []
    errors: list[dict] = []
    transitions = unavailable = stale = without_observation = 0
    body = encoded({"command": command})
    while not stop.is_set():
        started = time.perf_counter_ns()
        try:
            response, elapsed = request(endpoint, body, args.timeout)
            if not response["success"]:
                raise RuntimeError(str(response.get("error")))
            durations.append(elapsed)
            if command in ("get_snapshot", "get_metering"):
                observation = response.get("data", {}).get("observation", {})
                if observation:
                    transitions += bool(observation.get("transition_in_progress"))
                    unavailable += not observation.get("available", False)
                    stale += (observation.get("age_ms") or 0) > 0
                else:
                    without_observation += 1
        except Exception as error:
            errors.append({"elapsed_ns": time.perf_counter_ns() - started,
                           "error": f"{type(error).__name__}: {error}"})
        stop.wait(args.cadence_ms / 1000)
    return {"command": command, **distribution(durations), "errors": errors,
            "transition_responses": transitions, "unavailable_responses": unavailable,
            "cached_responses_with_positive_age": stale,
            "responses_without_observation_metadata": without_observation}


def control(endpoint: str, cold: bool, stop: threading.Event, args) -> dict:
    durations: list[int] = []
    errors: list[str] = []
    index = 0
    while not stop.is_set():
        command = ({"command": "apply_configuration", "sample_rate": (48000, 96000)[index % 2],
                    "buffer_frames": (64, 512)[index % 2]} if cold else
                   {"command": "set_volume", "volume": (0.25, 0.5)[index % 2]})
        index += 1
        try:
            _, elapsed = checked_request(endpoint, command, args.timeout)
            durations.append(elapsed)
        except Exception as error:
            errors.append(f"{type(error).__name__}: {error}")
        stop.wait(0.01 if not cold else 0.02)
    return {"kind": "cold_reconfiguration" if cold else "volume", **distribution(durations),
            "errors": errors}


def malformed(endpoint: str, stop: threading.Event, args) -> dict:
    durations: list[int] = []
    errors: list[str] = []
    while not stop.is_set():
        try:
            response, elapsed = request(endpoint, b'{"command":', args.timeout)
            if response["success"]:
                raise RuntimeError("malformed command was accepted")
            durations.append(elapsed)
        except Exception as error:
            errors.append(f"{type(error).__name__}: {error}")
        stop.wait(0.005)
    return {"kind": "malformed_requests", **distribution(durations), "errors": errors}


def keep_partial_clients(clients: list[socket.socket], stop: threading.Event) -> dict:
    durations: list[int] = []
    errors: list[str] = []
    while not stop.is_set():
        started = time.perf_counter_ns()
        try:
            # Extend the incomplete line without completing it. This keeps the
            # daemon's real read-idle clock live even for longer measurement runs.
            for client in clients:
                client.sendall(b" ")
            durations.append(time.perf_counter_ns() - started)
        except OSError as error:
            errors.append(f"{type(error).__name__}: {error}")
        stop.wait(1.0)
    return {"kind": "partial_client_keepalive_batches", **distribution(durations), "errors": errors}


def phase(endpoint: str, label: str, args) -> dict:
    stop = threading.Event()
    clients: list[socket.socket] = []
    started = time.perf_counter_ns()
    try:
        # Stay below the real 64-client admission bound. Every partial client
        # stays incomplete and receives keepalive bytes rather than timing out.
        if label == "partial_and_malformed":
            for _ in range(args.idle_clients):
                client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                clients.append(client)
                client.settimeout(args.timeout)
                client.connect(endpoint)
                client.sendall(b'{"command":')
        with concurrent.futures.ThreadPoolExecutor(max_workers=5) as executor:
            probes = [executor.submit(probe, endpoint, command, stop, args) for command in PROBES]
            workers = []
            if label in ("volume_churn", "cold_reconfiguration"):
                workers.append(executor.submit(control, endpoint, label == "cold_reconfiguration", stop, args))
            if label == "partial_and_malformed":
                workers.append(executor.submit(malformed, endpoint, stop, args))
                workers.append(executor.submit(keep_partial_clients, clients, stop))
            try:
                stop.wait(args.duration)
            finally:
                stop.set()
            result = {"phase": label, "probes": [future.result() for future in probes],
                      "workers": [future.result() for future in workers],
                      "partial_clients_held_to_end": sum(not select.select([c], [], [], 0)[0] for c in clients)}
    finally:
        stop.set()
        for client in clients:
            client.close()
    result.update({"wall_ns": time.perf_counter_ns() - started,
                   "partial_clients_opened": len(clients)})
    checked_request(endpoint, {"command": "ping"}, args.timeout)
    return result


def wait_for_audio(child: subprocess.Popen, endpoint: str, timeout: float, require_advance: bool = False) -> dict:
    deadline = time.monotonic() + timeout
    last = None
    baseline = None
    while time.monotonic() < deadline:
        if child.poll() is not None:
            raise RuntimeError(f"lab daemon exited during startup: {child.returncode}")
        try:
            response, _ = checked_request(endpoint, {"command": "get_snapshot"}, 1.0)
            last = response["data"]
            observed = last.get("observed", {})
            engine = observed.get("engine", {})
            driver = observed.get("driver", {})
            if (last.get("observation", {}).get("available")
                    and not last["observation"].get("transition_in_progress")
                    and engine.get("state") == "Playing"
                    and engine.get("playback_callback_count", 0) > 0
                    and engine.get("playback_frames_received", 0) > 0
                    and engine.get("playback_frames_written", 0) > 0
                    and driver.get("driver_ready") and driver.get("capture_active")
                    and not engine.get("last_error")
                    and not (isinstance(engine.get("playback_incident"), dict)
                             and engine["playback_incident"].get("resolution") is None)):
                if "Lab" not in driver.get("driver_name", ""):
                    raise RuntimeError("benchmark refused a non-lab capture backend")
                if engine.get("playback_output_device") != "Systemwide Lab Output":
                    raise RuntimeError("benchmark refused a non-lab output backend")
                if not require_advance:
                    return last
                current = {"pipeline_generation": last["generation"],
                           "output_generation": engine["playback_output_generation"],
                           "input": engine["playback_frames_received"],
                           "callbacks": engine["playback_callback_count"],
                           "output": engine["playback_frames_written"]}
                if (baseline is None or any(current[key] != baseline[key]
                                            for key in ("pipeline_generation", "output_generation"))):
                    baseline = current
                elif all(current[key] > baseline[key] for key in ("input", "callbacks", "output")):
                    return {"snapshot": last, "baseline": baseline, "advanced": current}
        except (OSError, KeyError, json.JSONDecodeError):
            pass
        time.sleep(0.02)
    raise RuntimeError(f"lab did not establish observed playback progress: {last}")


def run(args) -> dict:
    import resource

    binary = pathlib.Path(args.binary).resolve(strict=True)
    output = pathlib.Path(args.output).resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    source_before = selected_sources()
    report = {"created_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "host": platform.platform(), "profile": args.profile,
              "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "source_sha256": source_before, "configuration": vars(args),
              "host_load_average_start": os.getloadavg(),
              "scope": "Fresh Unix connection through complete JSON decode; real owned lab daemon.",
              "limits": ["No physical output/capture, native CoreAudio notifications or Windows authentication load.",
                         "Python scheduling, GIL, socket setup and JSON decoding are included in client timing.",
                         "No audio callback deadline or physical capture-to-output latency qualification.",
                         "The max response budget is a liveness guard; the metering p99 target uses the existing 5 ms handler budget as a stricter client-visible target.",
                         "No control response target certifies a realtime audio deadline."],
              "phases": [], "failures": []}
    runtime_parent = pathlib.Path(args.runtime_parent or binary.parent.parent)
    runtime_parent.mkdir(parents=True, exist_ok=True)
    stderr_path = output.with_suffix(".daemon.log")
    with tempfile.TemporaryDirectory(prefix="ipc-", dir=runtime_parent) as directory:
        runtime = pathlib.Path(directory).resolve()
        endpoint = str(runtime / "d.sock")
        if len(endpoint.encode()) > 100:
            raise RuntimeError("runtime endpoint is too long; pass a shorter --runtime-parent")
        environment = dict(os.environ)
        environment.pop("SOTF_OUTPUT_DEVICE", None)
        environment.update({"SOTF_SYSTEMWIDE_DRIVER": "lab", "SOTF_DEVICE_LISTENER": "0",
                            "SOTF_DAEMON_SOCKET_PATH": endpoint,
                            "SOTF_SYSTEMWIDE_RUNTIME_DIR": str(runtime),
                            "SOTF_SYSTEMWIDE_STATE_PATH": str(runtime / "state.json"),
                            "SOTF_DAEMON_SESSION_KEY_PATH": str(runtime / "session.key"),
                            "SOTF_HAL_SESSION_KEY_PATH": str(runtime / "hal" / "session.key"),
                            "SOTF_HAL_SHARED_MEMORY_PATH": str(runtime / "audio.shm"),
                            "RUST_LOG": "warn"})
        with stderr_path.open("wb") as stderr:
            child = subprocess.Popen([str(binary)], env=environment, stdout=subprocess.DEVNULL, stderr=stderr)
            try:
                report["startup"] = wait_for_audio(child, endpoint, args.startup_timeout)
                sequential = [checked_request(endpoint, {"command": "ping"}, args.timeout)[1]
                              for _ in range(args.sequential_samples)]
                report["sequential_fresh_ping"] = distribution(sequential)
                for label in ("steady", "volume_churn", "cold_reconfiguration", "partial_and_malformed"):
                    result = phase(endpoint, label, args)
                    report["phases"].append(result)
                    print(label, {p["command"]: {k: p.get(k) for k in ("count", "p99_ns", "max_ns")}
                                  for p in result["probes"]}, flush=True)
                report["post_load_progress"] = wait_for_audio(child, endpoint, args.startup_timeout, require_advance=True)
                report["final_snapshot"] = report["post_load_progress"]["snapshot"]
                report["final_command_telemetry"] = checked_request(endpoint, {"command": "dump_state"}, args.timeout)[0]["data"]["runtime_telemetry"]
                _, report["shutdown_roundtrip_ns"] = checked_request(endpoint, {"command": "shutdown"}, args.timeout)
                child.wait(timeout=10)
                report["daemon_exit_code"] = child.returncode
                if child.returncode != 0:
                    report["failures"].append(f"daemon exited with {child.returncode}")
            except Exception as error:
                report["failures"].append(f"{type(error).__name__}: {error}")
            finally:
                if child.poll() is None:
                    child.terminate()
                    try:
                        child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait()
    budget_ns = args.response_budget_ms * 1_000_000
    for result in report["phases"]:
        for probe_result in result["probes"]:
            if probe_result["count"] < 10 or probe_result["errors"]:
                report["failures"].append(f"{result['phase']}/{probe_result['command']}: missing/failed responses")
            if probe_result.get("max_ns", budget_ns + 1) > budget_ns:
                report["failures"].append(f"{result['phase']}/{probe_result['command']}: response liveness budget exceeded")
            if (probe_result["command"] == "get_metering"
                    and probe_result.get("p99_ns", math.inf) > args.metering_p99_budget_ms * 1_000_000):
                report["failures"].append(f"{result['phase']}/get_metering: client p99 target exceeded")
        for worker_result in result["workers"]:
            if worker_result["count"] == 0 or worker_result["errors"]:
                report["failures"].append(f"{result['phase']}/{worker_result['kind']}: load generator failed")
        if result["phase"] == "cold_reconfiguration" and not any(p["transition_responses"] for p in result["probes"]):
            report["failures"].append("cold reconfiguration never exercised the snapshot transition fallback")
        if result["partial_clients_held_to_end"] != result["partial_clients_opened"]:
            report["failures"].append("partial clients expired or were rejected during measurement")
    if selected_sources() != source_before:
        report["failures"].append("selected sources changed during measurement")
    report["daemon_log_sha256"] = hashlib.sha256(stderr_path.read_bytes()).hexdigest()
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    report["daemon_cpu_seconds"] = {"user": usage.ru_utime, "system": usage.ru_stime}
    report["daemon_maxrss_platform_native_units"] = usage.ru_maxrss
    report["host_load_average_end"] = os.getloadavg()
    output.write_text(json.dumps(report, indent=2) + "\n")
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--profile", default="unspecified")
    parser.add_argument("--runtime-parent")
    parser.add_argument("--duration", type=float, default=4.0)
    parser.add_argument("--cadence-ms", type=float, default=2.0)
    parser.add_argument("--idle-clients", type=int, default=24)
    parser.add_argument("--sequential-samples", type=int, default=200)
    parser.add_argument("--response-budget-ms", type=float, default=1000.0)
    parser.add_argument("--metering-p99-budget-ms", type=float, default=5.0)
    parser.add_argument("--timeout", type=float, default=3.0)
    parser.add_argument("--startup-timeout", type=float, default=20.0)
    args = parser.parse_args()
    if os.name != "posix" or not hasattr(socket, "AF_UNIX"):
        parser.error("this benchmark currently requires Unix IPC")
    if not 1 <= args.duration <= 60 or not 0 < args.cadence_ms <= 1000:
        parser.error("duration must be 1..60 seconds and cadence must be positive")
    if not 0 <= args.idle_clients <= 48 or args.sequential_samples < 20:
        parser.error("idle clients must be 0..48 and sequential samples must be at least 20")
    if not 0 < args.response_budget_ms <= args.timeout * 1000 or not 0 < args.startup_timeout <= 120:
        parser.error("invalid timeout or response budget")
    if not 0 < args.metering_p99_budget_ms <= args.response_budget_ms:
        parser.error("metering p99 target must be positive and no larger than the max response budget")
    report = run(args)
    print(f"Wrote {args.output}; failures={len(report['failures'])}", flush=True)
    for failure in report["failures"]:
        print(failure, flush=True)
    return 1 if report["failures"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
