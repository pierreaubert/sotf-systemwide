"""Capture qualification rejects incomplete data, inactive observers and source churn."""

from contextlib import redirect_stdout
import copy
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("capture_bench", ROOT / "scripts/bench_cpal_capture.py")
BENCH = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BENCH)


def fixture():
    return {"cases": [dict(format=sample_format, channels=channels, frames=frames, mode=mode,
        calls=2000, allocations=0, deallocations=0, first_callback_allocations=0,
        first_callback_deallocations=0, observer_canary_allocations=1, observer_canary_deallocations=1,
        p50_ns=10, p95_ns=20, p99_ns=30, max_ns=40)
        for sample_format in BENCH.FORMATS for channels in (2, 8, 32) for frames in (64, 512)
        for mode in ("drain", "overflow", "contended", "poisoned")]}


class CaptureVerifierTests(unittest.TestCase):
    def test_complete_matrix_is_accepted(self):
        BENCH.validate(fixture())

    def test_duplicates_missing_cases_and_inactive_or_allocating_observers_are_rejected(self):
        for change in (lambda rows: rows.pop(), lambda rows: rows.__setitem__(0, copy.deepcopy(rows[1])),
                lambda rows: rows[0].__setitem__("observer_canary_allocations", 0),
                lambda rows: rows[0].__setitem__("first_callback_allocations", 1),
                lambda rows: rows[0].__setitem__("deallocations", 1),
                lambda rows: rows[0].__setitem__("allocations", False),
                lambda rows: rows[0].__setitem__("p50_ns", False),
                lambda rows: rows[0].__setitem__("calls", 1999),
                lambda rows: rows[0].__setitem__("p99_ns", 50)):
            report = fixture()
            change(report["cases"])
            with self.subTest(case=report["cases"][0]), self.assertRaises(ValueError):
                BENCH.validate(report)

    def test_source_churn_removes_stale_qualification_and_keeps_snapshots(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            (output / "report.json").write_text("old qualification")

            def run(command, **kwargs):
                if command[0] == "cargo":
                    kwargs["stdout"].write(json.dumps({"reason": "compiler-artifact",
                        "target": {"name": "sotf-callback-metrics"},
                        "executable": str(output / "benchmark")}) + "\n")

            with patch.object(sys, "argv", ["bench", "--output-dir", directory]), \
                    patch.object(BENCH, "sources", side_effect=[{"capture.rs": "old"}, {"capture.rs": "new"}]), \
                    patch.object(BENCH.subprocess, "run", side_effect=run):
                with self.assertRaisesRegex(RuntimeError, "capture.rs.*no qualified report"):
                    BENCH.main()
            self.assertFalse((output / "report.json").exists())
            self.assertNotEqual((output / "sources-before.json").read_text(),
                                (output / "sources-after.json").read_text())

    def test_unicode_build_artifact_path_and_snapshot_hashes_survive_publication(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            executable = output / "mesuré 測定" / "callback.exe"
            executable.parent.mkdir()
            executable.write_bytes(b"unit-test executable")

            def run(command, **kwargs):
                if command[0] == "cargo":
                    self.assertEqual(kwargs["stdout"].encoding.lower(), "utf-8")
                    kwargs["stdout"].write(json.dumps({"reason": "compiler-artifact",
                        "target": {"name": "sotf-callback-metrics"}, "executable": str(executable)},
                        ensure_ascii=False) + "\n")
                else:
                    self.assertEqual(command[0], str(executable))
                    Path(command[1]).write_text(json.dumps(fixture()), encoding="utf-8")

            with patch.object(sys, "argv", ["bench", "--output-dir", directory]), \
                    patch.object(BENCH, "sources", return_value={"capture.rs": "stable"}), \
                    patch.object(BENCH.platform, "platform", return_value="unit-test"), \
                    patch.object(BENCH.subprocess, "check_output", return_value="rustc unit-test"), \
                    patch.object(BENCH.subprocess, "run", side_effect=run), redirect_stdout(io.StringIO()):
                BENCH.main()
            report = json.loads((output / "report.json").read_text(encoding="utf-8"))
            self.assertEqual(report["executable"], str(executable))
            self.assertEqual(report["source_snapshot_sha256"]["before"], report["source_snapshot_sha256"]["after"])


if __name__ == "__main__":
    unittest.main()
