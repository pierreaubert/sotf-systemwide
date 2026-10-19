"""Exercise unprivileged installer preparation and the generated readiness loop."""

import getpass
import os
import plistlib
import shutil
import socket
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


@unittest.skipUnless(sys.platform == "darwin" and os.geteuid() != 0, "macOS console-user fixture")
class InstallerUserFilesTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="sotf-installer-")
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.templates = self.directory / "templates"
        self.templates.mkdir()
        for label in ("org.spinorama.sotf-daemon", "org.spinorama.sotf-systemwide"):
            shutil.copy(ROOT / "builds/macos" / f"{label}.plist", self.templates)
        self.agents = self.directory / "home/Library/LaunchAgents"
        self.logs = self.directory / "home/Library/Logs/SotF"

    def prepare(self):
        return subprocess.run(
            ["/bin/bash", str(ROOT / "scripts/lib/prepare_user_files.sh"),
             str(self.templates), str(self.agents), str(self.logs), "8"],
            text=True, capture_output=True, check=False,
        )

    def test_creates_private_logs_and_rolls_existing_content(self):
        self.assertEqual(self.prepare().returncode, 0)
        log = self.logs / "sotf-daemon.log"
        log.write_text("previous contents")
        self.assertEqual(self.prepare().returncode, 0)
        self.assertEqual(log.read_text(), "")
        self.assertEqual(log.with_suffix(".log.1").read_text(), "previous contents")
        self.assertEqual(log.stat().st_mode & 0o777, 0o600)
        for agent in self.agents.glob("*.plist"):
            contents = plistlib.loads(agent.read_bytes())
            self.assertTrue(contents["StandardOutPath"].startswith(str(self.logs)))
            self.assertNotIn(b"__SOTF_", agent.read_bytes())

    def test_log_and_rollover_symlinks_cannot_modify_the_target(self):
        for suffix in ("", ".1"):
            with self.subTest(suffix=suffix):
                self.logs.mkdir(parents=True, exist_ok=True)
                target = self.directory / "protected"
                target.write_text("unchanged")
                target.chmod(0o640)
                link = self.logs / f"sotf-daemon.log{suffix}"
                link.symlink_to(target)
                self.assertNotEqual(self.prepare().returncode, 0)
                self.assertEqual(target.read_text(), "unchanged")
                self.assertEqual(target.stat().st_mode & 0o777, 0o640)
                link.unlink()

    def test_rejects_symlinked_destination_directory(self):
        target = self.directory / "elsewhere"
        target.mkdir()
        self.logs.parent.mkdir(parents=True)
        self.logs.symlink_to(target, target_is_directory=True)
        self.assertNotEqual(self.prepare().returncode, 0)
        self.assertEqual(list(target.iterdir()), [])

    def test_readiness_message_emitted_before_bootstrap_returns_is_seen(self):
        scripts = self.directory / "package-scripts"
        scripts.mkdir()
        (scripts / "installer-common.sh").write_text("installer_step() { :; }\n")
        shutil.copy(ROOT / "scripts/lib/prepare_user_files.sh", scripts)
        endpoint = self.directory / "daemon.sock"
        listener = socket.socket(socket.AF_UNIX)
        self.addCleanup(listener.close)
        listener.bind(str(endpoint))
        # The smoke client verifies which endpoint was discovered. No installed
        # launchd job or real audio endpoint participates in this fixture.
        (scripts / "daemon_smoke.py").write_text(
            "import sys\nassert sys.argv[1] == " + repr(str(endpoint)) + "\n"
        )
        launcher = scripts / "launchctl"
        launcher.write_text(
            '#!/bin/bash\nif [ "$1" = bootstrap ]; then\n'
            f'  echo "Audio daemon listening on {endpoint}" >> "{self.logs}/sotf-daemon.log"\n'
            'fi\nexit 0\n'
        )
        launcher.chmod(0o700)
        source = (ROOT / "scripts/build-systemwide.sh").read_text()
        start = source.index("<< 'POSTINSTALL'\n") + len("<< 'POSTINSTALL'\n")
        script = source[start:source.index("\nPOSTINSTALL", start)]
        lines = []
        for line in script.splitlines():
            if line.startswith("CONSOLE_USER="):
                line = "CONSOLE_USER=" + getpass.getuser()
            elif line.startswith("USER_HOME="):
                line = f'USER_HOME="{self.directory}/home"'
            elif line.startswith("AGENT_TEMPLATE_DIR="):
                line = f'AGENT_TEMPLATE_DIR="{self.templates}"'
            lines.append(line)
        script = "\n".join(lines).replace('/usr/bin/sudo -u "$CONSOLE_USER" -- "$@"', '"$@"')
        script = script.replace("/bin/launchctl", str(launcher))
        postinstall = scripts / "postinstall"
        postinstall.write_text(script)
        result = subprocess.run(["/bin/bash", str(postinstall)], text=True,
                                capture_output=True, timeout=10, check=False)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f"Daemon control socket: {endpoint}", result.stdout)


if __name__ == "__main__":
    unittest.main()
