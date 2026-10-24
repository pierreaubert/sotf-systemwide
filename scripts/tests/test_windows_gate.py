"""Portable control-flow tests; these do not execute Windows ACLs or IPC."""

from contextlib import redirect_stderr, redirect_stdout
import ctypes
import hashlib
import importlib.util
import io
import json
from pathlib import Path
from types import SimpleNamespace
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("windows_gate", ROOT / "scripts/test_systemwide_windows.py")
GATE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GATE)


def measurements(output):
    for name, count in (("capture-closure", 288), ("lab-latency", 54)):
        directory = output / name
        directory.mkdir(parents=True)
        snapshot = b"{}\n"
        for moment in ("before", "after"):
            (directory / f"sources-{moment}.json").write_bytes(snapshot)
        digest = hashlib.sha256(snapshot).hexdigest()
        (directory / "report.json").write_text(json.dumps({"cases": [{}] * count,
            "binary_sha256": "0" * 64, "source_snapshot_sha256": {"before": digest, "after": digest}}))


class WindowsGateTests(unittest.TestCase):
    def test_wine_and_unsupported_python_are_refused(self):
        with patch.object(sys, "platform", "win32"), \
                patch.object(ctypes, "WinDLL", create=True,
                             return_value=SimpleNamespace(wine_get_version=object())):
            with self.assertRaisesRegex(RuntimeError, "Wine"):
                GATE.require_native_windows()
        with patch.object(sys, "platform", "win32"), patch.object(sys, "version_info", (3, 10)), \
                patch.object(ctypes, "WinDLL", create=True, return_value=SimpleNamespace()):
            with self.assertRaisesRegex(RuntimeError, "3.11"):
                GATE.require_native_windows()

    def test_non_windows_refusal_precedes_all_output_and_processes(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "must-not-exist"
            with patch.object(sys, "platform", "darwin"), \
                    patch.object(sys, "argv", ["gate", "--output-dir", str(output)]), \
                    patch.object(GATE.subprocess, "run") as run:
                with self.assertRaisesRegex(RuntimeError, "requires native Windows"):
                    GATE.main()
            run.assert_not_called()
            self.assertFalse(output.exists())

    def invoke(self, output, code, snapshots):
        def run(command, **kwargs):
            self.assertEqual(kwargs["env"]["SOTF_SYSTEMWIDE_DRIVER"], "lab")
            runtime = Path(kwargs["env"]["SOTF_SYSTEMWIDE_RUNTIME_DIR"])
            self.assertEqual(runtime.parent, output.resolve())
            kwargs["stdout"].write("mocked step output\n")
            return SimpleNamespace(returncode=code)

        with patch.object(sys, "argv", ["gate", "--output-dir", str(output)]), \
                patch.object(GATE, "require_native_windows"), \
                patch.object(GATE.platform, "platform", return_value="mocked-control-flow"), \
                patch.object(GATE, "sources", side_effect=snapshots), \
                patch.object(GATE, "steps", return_value=[("mock-step", ["mock-command"])]), \
                patch.object(GATE.subprocess, "run", side_effect=run), \
                redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
            return GATE.main()

    def test_failed_step_replaces_old_green_report(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            (output / "report.json").write_text('{"component_gate_passed": true}')
            self.assertEqual(self.invoke(output, 7, [{"daemon.rs": "before"}]), 1)
            report = json.loads((output / "report.json").read_text())
            self.assertFalse(report["component_gate_passed"])
            self.assertEqual(report["steps"][0]["exit_code"], 7)

    def test_source_change_cannot_qualify_successful_steps(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            measurements(output)
            self.assertEqual(self.invoke(output, 0, [{"daemon.rs": "before"}, {"daemon.rs": "after"}]), 1)
            report = json.loads((output / "report.json").read_text())
            self.assertFalse(report["component_gate_passed"])
            self.assertEqual(report["source_changes"], ["daemon.rs"])

    def test_component_success_preserves_outstanding_acceptance(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            measurements(output)
            self.assertEqual(self.invoke(output, 0, [{"daemon.rs": "same"}] * 2), 0)
            report = json.loads((output / "report.json").read_text())
            self.assertTrue(report["component_gate_passed"])
            self.assertFalse(report["audit_complete"])
            self.assertEqual(report["measurement_reports"]["capture-closure"]["case_count"], 288)
            self.assertIn("two-account", report["remaining_acceptance"][0])


if __name__ == "__main__":
    unittest.main()
