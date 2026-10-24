#!/usr/bin/env python3
"""Post-install smoke test for the SotF daemon.

Polls the freshly installed daemon until playback is applied, the daemon
reports a terminal fault, or the budget expires. Every poll prints a progress
line so the Installer log (Cmd+L) never looks wedged.

Exit codes (chosen for the pkg postinstall wrapper):
  0  install may proceed. Either playback is healthy, or the daemon is
     responsive and the only blocker is a powered-off/unplugged desired
     output device; that case prints a loud WARNING and ConfigBar surfaces
     the fault at runtime.
  1  install must fail. Prints a diagnostic naming the observed state.

Also importable for unit tests: see scripts/tests/test_daemon_smoke.py.
Stdlib only: the installer target has /usr/bin/python3 and nothing else.
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import sys
import time


# Mirrors the daemon's cold-start retry markers
# (startup_error_is_device_availability in crates/daemon/.../misc.rs).
# A latched fault containing one of these is a device-availability race or a
# genuinely offline device, never a broken build on its own.
DEVICE_AVAILABILITY_MARKERS = (
    "not available",
    "not found",
    "needs playback stream recovery",
    "stream error reported",
    "device id changed",
)

QUERY_TIMEOUT = 15.0
PING_TIMEOUT = 10.0


def query(socket_path: str, command: dict, timeout: float = QUERY_TIMEOUT) -> dict:
    """Send one JSON command line and return the decoded JSON response."""
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        client.settimeout(timeout)
        client.connect(socket_path)
        client.sendall((json.dumps(command) + "\n").encode())
        data = b""
        while not data.endswith(b"\n"):
            chunk = client.recv(65536)
            if not chunk:
                break
            data += chunk
    finally:
        client.close()
    return json.loads(data.decode())


def is_device_availability_error(text: str | None) -> bool:
    if not text:
        return False
    folded = text.lower()
    return any(marker in folded for marker in DEVICE_AVAILABILITY_MARKERS)


def _status_field(status: dict, *path: str) -> object:
    node: object = status
    for key in path:
        if not isinstance(node, dict):
            return None
        node = node.get(key)
    return node


def _status_str(status: dict, *path: str) -> str | None:
    node = _status_field(status, *path)
    return node if isinstance(node, str) else None


def applied_output_device(status: dict) -> str | None:
    return _status_str(status, "active_route", "applied_output_device")


def desired_output_device(status: dict) -> str | None:
    return _status_str(
        status, "active_route", "desired_output_device"
    ) or _status_str(status, "selected_device")


def pipeline_recovery(status: dict) -> dict | None:
    recovery = _status_field(status, "pipeline_recovery")
    return recovery if isinstance(recovery, dict) else None


def classify(status: dict, device_names: set[str] | None) -> tuple[str, str]:
    """Classify one status poll.

    Returns (class, detail) where class is one of:
      healthy         playback applied, no active incident or fatal/recovery latch
      missing_device  latched device-availability fault AND the desired
                      device is provably absent from enumeration
      fault           latched fault that is not a provable device absence
      settling        no latch yet (daemon still starting or retrying)
    """
    recovery = pipeline_recovery(status)
    applied = applied_output_device(status)
    engine_error = _status_str(status, "last_error")
    if engine_error:
        return ("fault", f"engine failure latched: {engine_error}")
    incident = _status_field(status, "playback_incident")
    if not recovery and isinstance(incident, dict) and incident.get("resolution") is None:
        return (
            "settling",
            f"output recovery active ({incident.get('kind')}, incident {incident.get('id')})",
        )
    if applied and not recovery:
        return ("healthy", f"playback applied on {applied!r} with no recovery latch")
    if not recovery:
        return ("settling", f"no applied output yet (state={status.get('state')!r})")
    error = str(recovery.get("error") or "")
    desired = desired_output_device(status)
    if (
        is_device_availability_error(error)
        and desired
        and device_names is not None
        and desired not in device_names
    ):
        return ("missing_device", f"desired output {desired!r} is offline: {error}")
    kind = "device-availability" if is_device_availability_error(error) else "terminal"
    return ("fault", f"{kind} fault latched: {error}")


def list_device_names(socket_path: str) -> set[str] | None:
    """Enumerate output-device names via the daemon. None when unknowable."""
    try:
        response = query(socket_path, {"command": "list_devices"})
    except Exception:
        return None
    if not response.get("success"):
        return None
    data = response.get("data")
    devices = data.get("devices") if isinstance(data, dict) else None
    if not isinstance(devices, list):
        return None
    names: set[str] = set()
    for device in devices:
        if not isinstance(device, dict):
            continue
        name = device.get("name")
        if isinstance(name, str):
            names.add(name)
    return names


def wait_for_ping(socket_path: str, attempts: int = 3) -> None:
    """Raise RuntimeError unless the daemon answers ping within the attempts."""
    last_error: Exception | None = None
    for _ in range(attempts):
        try:
            query(socket_path, {"command": "ping"}, timeout=PING_TIMEOUT)
            return
        except Exception as error:  # noqa: BLE001 - any failure means retry
            last_error = error
            time.sleep(2)
    raise RuntimeError(f"daemon did not answer ping: {last_error}")


def poll(
    socket_path: str,
    budget_secs: float,
    interval_secs: float,
    confirm_polls: int,
) -> int:
    """Run the smoke poll loop. Returns the process exit code."""
    print(f"Daemon control socket: {socket_path}", flush=True)
    try:
        wait_for_ping(socket_path)
    except RuntimeError as error:
        print(f"SMOKE FAIL: {error}", flush=True)
        return 1
    print("Daemon answered ping; polling playback status...", flush=True)

    deadline = time.time() + budget_secs
    start = time.time()
    last_class = ""
    last_detail = ""
    consecutive = 0
    last_status: dict = {}

    while time.time() < deadline:
        elapsed = time.time() - start
        try:
            response = query(socket_path, {"command": "status"})
            if not response.get("success"):
                raise RuntimeError(f"status query failed: {response}")
            last_status = response.get("data") or {}
        except Exception as error:  # noqa: BLE001 - daemon may be mid-restart
            print(
                f"[smoke {elapsed:4.0f}s/{budget_secs:.0f}s] "
                f"status query failed ({error}); retrying...",
                flush=True,
            )
            if not os.path.exists(socket_path):
                print(
                    "SMOKE FAIL: daemon socket disappeared; "
                    "the daemon process likely crashed. "
                    "See the daemon log for details.",
                    flush=True,
                )
                return 1
            consecutive = 0
            time.sleep(interval_secs)
            continue

        device_names: set[str] | None = None
        if pipeline_recovery(last_status):
            # Only a latched fault needs enumeration; a healthy daemon
            # answers from the status payload alone.
            device_names = list_device_names(socket_path)
        class_, detail = classify(last_status, device_names)
        if class_ == last_class and detail == last_detail:
            consecutive += 1
        else:
            consecutive = 1
        last_class, last_detail = class_, detail
        print(
            f"[smoke {elapsed:4.0f}s/{budget_secs:.0f}s] {class_}: {detail}",
            flush=True,
        )

        if class_ == "healthy":
            print(f"SMOKE PASS: {detail}", flush=True)
            return 0
        if class_ == "missing_device" and consecutive >= confirm_polls:
            print(
                "SMOKE WARNING: daemon is healthy but the desired output "
                f"device is offline ({detail}). Install continues; power the "
                "device on or select another output in SotF Systemwide.",
                flush=True,
            )
            return 0
        if class_ == "fault" and consecutive >= confirm_polls:
            print(f"SMOKE FAIL: {detail}", flush=True)
            print(f"Last status: {json.dumps(last_status)}", flush=True)
            return 1
        time.sleep(interval_secs)

    # Budget exhausted: give a settling daemon no more time, but re-check the
    # final state once so a late recovery or a provable device absence is
    # reported accurately instead of as a bare timeout.
    class_, detail = classify(last_status, list_device_names(socket_path))
    if class_ == "healthy":
        print(f"SMOKE PASS: {detail}", flush=True)
        return 0
    if class_ == "missing_device":
        print(
            "SMOKE WARNING: daemon is healthy but the desired output "
            f"device is offline ({detail}). Install continues; power the "
            "device on or select another output in SotF Systemwide.",
            flush=True,
        )
        return 0
    print(f"SMOKE FAIL: daemon did not reach healthy playback: {detail}", flush=True)
    print(f"Last status: {json.dumps(last_status)}", flush=True)
    return 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="SotF daemon post-install smoke test")
    parser.add_argument("socket", help="daemon control socket path")
    parser.add_argument(
        "--budget",
        type=float,
        default=90.0,
        help="max seconds to wait for healthy playback (default: 90)",
    )
    parser.add_argument(
        "--interval",
        type=float,
        default=5.0,
        help="seconds between status polls (default: 5)",
    )
    parser.add_argument(
        "--confirm",
        type=int,
        default=3,
        help="consecutive identical latched polls before deciding (default: 3)",
    )
    args = parser.parse_args(argv)
    return poll(args.socket, args.budget, args.interval, args.confirm)


if __name__ == "__main__":
    sys.exit(main())
