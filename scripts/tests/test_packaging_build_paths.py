"""Exercise packaging build paths without building or installing payloads."""

import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class PackagingBuildPathTests(unittest.TestCase):
    def test_native_builds_use_cargo_output_and_swift_dist_directory(self) -> None:
        source = (ROOT / "scripts/build-systemwide.sh").read_text()
        initialization = re.search(r'^BUILD_DIR=.*$', source, re.MULTILINE).group()
        function = re.search(
            r'^build_components\(\) \{\n.*?^\}', source, re.MULTILINE | re.DOTALL
        ).group()
        for profile in ("dist", "debug"):
            for custom_target in (False, True):
                with self.subTest(profile=profile, custom_target=custom_target):
                    with tempfile.TemporaryDirectory() as directory:
                        project = Path(directory) / "project"
                        project.mkdir()
                        target = (
                            Path(directory) / "custom cargo target"
                            if custom_target else project / "target"
                        )
                        env = os.environ.copy()
                        env.update(
                            PROJECT_ROOT=str(project),
                            CONFIGBAR_DIR=str(project / "swift/configbar"),
                            BUILD_TYPE=profile,
                            DEBUG=str(profile == "debug").lower(),
                            BUILD_HAL="false",
                            DAEMON_BINARY="sotf-daemon",
                            SYSTEMWIDE_BINARY="sotf-systemwide",
                            TEST_TARGET=str(target),
                            TEST_METADATA=json.dumps({"target_directory": str(target)}),
                        )
                        harness = r'''
set -euo pipefail
log_info() { :; }
log_success() { :; }
log_error() { echo "$1" >&2; }
write_daemon() {
    mkdir -p "$TEST_TARGET/$BUILD_TYPE"
    echo current-daemon > "$TEST_TARGET/$BUILD_TYPE/sotf-daemon"
}
cargo() {
    case "$1" in
        metadata) echo "$TEST_METADATA" ;;
        build) write_daemon ;;
        *) return 99 ;;
    esac
}
swift() { :; }
just() {
    case "$1" in
        dist-daemon) write_daemon ;;
        dist-systemwide)
            mkdir -p "$PROJECT_ROOT/target/dist"
            echo current-app > "$PROJECT_ROOT/target/dist/sotf-systemwide"
            ;;
        *) return 99 ;;
    esac
}
'''
                        result = subprocess.run(
                            ["/bin/bash", "-c", harness + initialization + "\n" + function
                             + "\nbuild_components\n"],
                            env=env, capture_output=True, text=True, check=False,
                        )
                        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                        self.assertEqual(
                            (target / profile / "sotf-daemon").read_text(), "current-daemon\n"
                        )


if __name__ == "__main__":
    unittest.main()
