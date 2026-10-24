"""Verify that source churn cannot publish or retain a qualified latency report."""

import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("lab_latency_bench", ROOT / "scripts/bench_lab_latency.py")
BENCH = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BENCH)


class ProvenanceTests(unittest.TestCase):
    def test_source_churn_retains_snapshots_and_removes_stale_report(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            (output / "report.json").write_text("stale qualified report")
            (output / "sources-after.json").write_text("stale source snapshot")

            def run(command, **kwargs):
                if command[0] == "cargo":
                    kwargs["stdout"].write(json.dumps({
                        "reason": "compiler-artifact", "target": {"name": "lab_latency_benchmark"},
                        "executable": str(output / "benchmark"),
                    }) + "\n")

            with patch.object(sys, "argv", ["bench", "--output-dir", directory]), \
                    patch.object(BENCH, "sources", side_effect=[{"engine.rs": "before"}, {"engine.rs": "after"}]), \
                    patch.object(BENCH.subprocess, "run", side_effect=run):
                with self.assertRaisesRegex(RuntimeError, "engine.rs.*no verified aggregate"):
                    BENCH.main()

            self.assertFalse((output / "report.json").exists())
            self.assertEqual(json.loads((output / "sources-before.json").read_text()), {"engine.rs": "before"})
            self.assertEqual(json.loads((output / "sources-after.json").read_text()), {"engine.rs": "after"})


if __name__ == "__main__":
    unittest.main()
