"""Unit tests for scripts/lib/daemon_smoke.py.

Run from the repo root:  python3 scripts/tests/test_daemon_smoke.py

Socket-level tests bind throwaway Unix sockets; they skip cleanly where the
platform forbids bind (sandboxed runs). The pure classifier tests always run.
"""

import contextlib
import io
import json
import os
import shutil
import socket
import sys
import tempfile
import threading
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))

import daemon_smoke  # pyright: ignore[reportMissingImports] - dynamic sys.path above


def _can_bind_unix_socket() -> bool:
    directory = tempfile.mkdtemp(prefix="smoke-bind-probe-")
    try:
        probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            probe.bind(os.path.join(directory, "probe.sock"))
        finally:
            probe.close()
        return True
    except OSError:
        return False
    finally:
        shutil.rmtree(directory, ignore_errors=True)


CAN_BIND = _can_bind_unix_socket()

ROOT = Path(__file__).resolve().parent.parent.parent


def _status(
    applied=None,
    recovery=None,
    desired="EVO8",
    state="Idle",
):
    return {
        "state": state,
        "selected_device": desired,
        "active_route": {
            "applied_output_device": applied,
            "desired_output_device": desired,
        },
        "pipeline_recovery": recovery,
        "recovery_actions": ["restart_daemon"],
    }


class ClassifyTests(unittest.TestCase):
    def test_active_output_incident_cannot_pass_from_an_applied_route(self) -> None:
        status = _status(applied="Mac mini Speakers", state="Playing")
        status["playback_incident"] = {
            "id": 100,
            "stream_generation": 41,
            "kind": "stream_error",
            "message": "arbitrary localized diagnostic",
            "resolution": None,
        }
        class_, detail = daemon_smoke.classify(status, None)
        self.assertEqual(class_, "settling")
        self.assertIn("stream_error", detail)

    def test_resolved_output_incident_is_history_and_does_not_hide_fatal_error(self) -> None:
        status = _status(applied="Mac mini Speakers", state="Playing")
        status["playback_incident"] = {
            "id": 100,
            "stream_generation": 41,
            "kind": "stream_error",
            "message": "arbitrary localized diagnostic",
            "resolution": {"output_generation": 42, "callback_count": 9},
        }
        class_, _ = daemon_smoke.classify(status, None)
        self.assertEqual(class_, "healthy")
        status["last_error"] = "decoder failed"
        class_, detail = daemon_smoke.classify(status, None)
        self.assertEqual(class_, "fault")
        self.assertIn("decoder failed", detail)

    def test_healthy_when_applied_with_no_latch(self) -> None:
        class_, detail = daemon_smoke.classify(
            _status(applied="Mac mini Speakers"), {"Mac mini Speakers"}
        )
        self.assertEqual(class_, "healthy")
        self.assertIn("Mac mini Speakers", detail)

    def test_settling_when_starting_without_latch(self) -> None:
        class_, _ = daemon_smoke.classify(_status(), {"EVO8"})
        self.assertEqual(class_, "settling")

    def test_missing_device_when_desired_is_provably_absent(self) -> None:
        status = _status(
            recovery={
                "error": "Selected output device 'EVO8' is not available",
                "actions": ["restart_daemon"],
            }
        )
        class_, detail = daemon_smoke.classify(status, {"Mac mini Speakers"})
        self.assertEqual(class_, "missing_device")
        self.assertIn("EVO8", detail)

    def test_missing_device_requires_enumeration_proof(self) -> None:
        status = _status(
            recovery={
                "error": "Selected output device 'EVO8' is not available",
                "actions": ["restart_daemon"],
            }
        )
        # Enumeration failed: never warn-pass on an unproven absence.
        class_, _ = daemon_smoke.classify(status, None)
        self.assertEqual(class_, "fault")

    def test_device_present_with_markers_is_a_fault(self) -> None:
        # The daemon clears its latch while a retry is pending, so a
        # persistent markers-match with the device present means the daemon
        # gave up: fail (after confirmation polls) instead of warn-passing.
        status = _status(
            recovery={
                "error": "CoreAudio device id changed for 'EVO8'",
                "actions": ["restart_daemon"],
            }
        )
        class_, _ = daemon_smoke.classify(status, {"EVO8"})
        self.assertEqual(class_, "fault")

    def test_generic_latch_is_a_fault(self) -> None:
        status = _status(
            recovery={
                "error": "Failed to load plugin chain: boom",
                "actions": ["restart_daemon"],
            }
        )
        class_, detail = daemon_smoke.classify(status, set())
        self.assertEqual(class_, "fault")
        self.assertIn("boom", detail)

    def test_marker_parity_with_daemon_matcher(self) -> None:
        # Keep in sync with startup_error_is_device_availability.
        for marker in (
            "Selected output device 'EVO8' is not available",
            "Audio device 'EVO8' not found",
            "Audio device 'EVO8' needs playback stream recovery",
            "stream error reported by CoreAudio",
            "CoreAudio device id changed for 'EVO8'",
        ):
            self.assertTrue(
                daemon_smoke.is_device_availability_error(marker), marker
            )
        self.assertFalse(daemon_smoke.is_device_availability_error(None))
        self.assertFalse(daemon_smoke.is_device_availability_error(""))
        self.assertFalse(
            daemon_smoke.is_device_availability_error(
                "Failed to load plugin chain: Engine thread exited unexpectedly"
            )
        )


class FakeDaemon(threading.Thread):
    """Minimal scripted daemon: answers one JSON line per connection."""

    def __init__(self, handler) -> None:
        super().__init__(daemon=True)
        self.handler = handler
        self.directory = tempfile.mkdtemp(prefix="smoke-fake-daemon-")
        self.socket_path = os.path.join(self.directory, "daemon.sock")
        self._stop = threading.Event()
        self._ready = threading.Event()

    def run(self) -> None:
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        server.bind(self.socket_path)
        server.listen(16)
        server.settimeout(0.2)
        self._ready.set()
        try:
            while not self._stop.is_set():
                try:
                    conn, _ = server.accept()
                except socket.timeout:
                    continue
                with conn:
                    conn.settimeout(5.0)
                    data = b""
                    while not data.endswith(b"\n"):
                        chunk = conn.recv(65536)
                        if not chunk:
                            break
                        data += chunk
                    try:
                        command = json.loads(data.decode() or "{}")
                    except json.JSONDecodeError:
                        command = {}
                    response = self.handler(command)
                    conn.sendall((json.dumps(response) + "\n").encode())
        finally:
            server.close()

    def __enter__(self) -> "FakeDaemon":
        self.start()
        if not self._ready.wait(timeout=10):
            raise RuntimeError("fake daemon did not bind in time")
        return self

    def __exit__(self, *args) -> None:
        self._stop.set()
        self.join(timeout=10)
        shutil.rmtree(self.directory, ignore_errors=True)


def _scripted_handler(status_payload, devices):
    def handle(command):
        name = command.get("command")
        if name == "ping":
            return {"success": True, "data": {}}
        if name == "status":
            return {"success": True, "data": status_payload}
        if name == "list_devices":
            return {
                "success": True,
                "data": {
                    "generation": 1,
                    "devices": [{"name": name} for name in devices],
                },
            }
        return {"success": False, "error": f"unexpected command {name!r}"}

    return handle


@unittest.skipUnless(CAN_BIND, "platform forbids Unix socket bind")
class PollTests(unittest.TestCase):
    def _run_poll(self, daemon, **kwargs):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = daemon_smoke.poll(
                daemon.socket_path,
                kwargs.get("budget", 3.0),
                kwargs.get("interval", 0.05),
                kwargs.get("confirm", 2),
            )
        return code, output.getvalue()

    def test_healthy_passes_immediately(self) -> None:
        status = _status(applied="Mac mini Speakers", state="Playing")
        with FakeDaemon(_scripted_handler(status, ["Mac mini Speakers"])) as daemon:
            code, output = self._run_poll(daemon)
        self.assertEqual(code, 0)
        self.assertIn("SMOKE PASS", output)

    def test_missing_device_warn_passes(self) -> None:
        status = _status(
            recovery={
                "error": "Selected output device 'EVO8' is not available",
                "actions": ["restart_daemon"],
            }
        )
        with FakeDaemon(_scripted_handler(status, ["Mac mini Speakers"])) as daemon:
            code, output = self._run_poll(daemon)
        self.assertEqual(code, 0)
        self.assertIn("SMOKE WARNING", output)
        self.assertIn("EVO8", output)

    def test_fatal_latch_fails_fast(self) -> None:
        status = _status(
            recovery={
                "error": "Failed to load plugin chain: boom",
                "actions": ["restart_daemon"],
            }
        )
        with FakeDaemon(_scripted_handler(status, ["EVO8"])) as daemon:
            code, output = self._run_poll(daemon)
        self.assertEqual(code, 1)
        self.assertIn("SMOKE FAIL", output)
        self.assertIn("boom", output)

    def test_settling_forever_fails_at_budget(self) -> None:
        with FakeDaemon(_scripted_handler(_status(), ["EVO8"])) as daemon:
            code, output = self._run_poll(daemon, budget=0.3)
        self.assertEqual(code, 1)
        self.assertIn("SMOKE FAIL", output)
        self.assertIn("[smoke", output)  # progress was visible while waiting

    def test_progress_lines_stream_while_polling(self) -> None:
        status = _status(
            recovery={
                "error": "Failed to load plugin chain: boom",
                "actions": ["restart_daemon"],
            }
        )
        with FakeDaemon(_scripted_handler(status, ["EVO8"])) as daemon:
            _, output = self._run_poll(daemon, confirm=3)
        self.assertGreaterEqual(output.count("[smoke"), 3)


class UnreachableTests(unittest.TestCase):
    def test_unreachable_socket_fails(self) -> None:
        directory = tempfile.mkdtemp(prefix="smoke-unreachable-")
        try:
            missing = os.path.join(directory, "daemon.sock")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                code = daemon_smoke.poll(missing, 1.0, 0.05, 1)
            self.assertEqual(code, 1)
            self.assertIn("SMOKE FAIL", output.getvalue())
        finally:
            shutil.rmtree(directory, ignore_errors=True)


class PackagingTests(unittest.TestCase):
    def test_postinstall_stages_the_smoke_script(self) -> None:
        build = (ROOT / "scripts" / "build-systemwide.sh").read_text()
        self.assertIn("daemon_smoke.py", build)
        self.assertIn('"$pkg_scripts/daemon_smoke.py"', build)
        self.assertIn('"$SCRIPT_DIR/daemon_smoke.py"', build)

    def test_hal_component_installs_before_the_app(self) -> None:
        # Install order is the outline order: the HAL payload must land and
        # CoreAudio must restart before the app component starts the daemon,
        # so the daemon smoke test validates the final system state.
        build = (ROOT / "scripts" / "build-systemwide.sh").read_text()
        hal_line = build.index('<line choice="$HAL_BUNDLE_ID"/>')
        app_line = build.index('<line choice="$SYSTEMWIDE_BUNDLE_ID"/>')
        launch_line = build.index('<line choice="org.spinorama.sotf.autolaunch"/>')
        self.assertLess(hal_line, app_line)
        self.assertLess(app_line, launch_line)
        hal_choice = build.index('<choice id="$HAL_BUNDLE_ID"')
        app_choice = build.index('<choice id="$SYSTEMWIDE_BUNDLE_ID"')
        self.assertLess(hal_choice, app_choice)


if __name__ == "__main__":
    unittest.main(verbosity=2)
