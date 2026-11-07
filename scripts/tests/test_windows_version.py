"""Windows packaging must use the workspace (installer) version.

macOS/Linux get it from the root Cargo.toml via `sotf_version`
(scripts/lib/common.sh). The Windows build script once read the daemon
crate version (0.1.x) instead, shipping 0.1.x-named Windows artifacts
for a 0.7.x release. Daemon and installer versions are intentionally
independent (see CHANGELOG.md); only the packaging source was wrong.
"""

from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]


def _section(toml: str, header: str) -> str:
    match = re.search(
        rf"(?s)\[{re.escape(header)}\](.*?)(?=\n\[|\Z)", toml
    )
    assert match is not None, f"missing [{header}] in Cargo.toml"
    return match.group(1)


def _version(section: str) -> str:
    match = re.search(r'(?m)^version\s*=\s*"([^"]+)"', section)
    assert match is not None, "missing version in section"
    return match.group(1)


class WindowsVersionTests(unittest.TestCase):
    def test_windows_build_reads_workspace_version_not_daemon(self) -> None:
        ps1 = (ROOT / "scripts/build-windows.ps1").read_text()

        self.assertIn('Join-Path $ProjectRoot "Cargo.toml"', ps1)
        self.assertIn("[workspace.package]", ps1)
        self.assertNotIn("DaemonToml", ps1)
        self.assertNotIn('crates\\daemon\\Cargo.toml', ps1)

    def test_extracted_version_matches_macos_linux_source(self) -> None:
        root_toml = (ROOT / "Cargo.toml").read_text()
        workspace_version = _version(_section(root_toml, "workspace.package"))

        # sotf_version parity: `grep -m1 '^version = '` on the root file.
        first = re.search(r'(?m)^version\s*=\s*"([^"]+)"', root_toml)
        self.assertIsNotNone(first)
        self.assertEqual(workspace_version, first.group(1))

        # The daemon crate version is a different, independent track.
        daemon_toml = (ROOT / "crates/daemon/Cargo.toml").read_text()
        daemon_version = _version(daemon_toml)
        self.assertNotEqual(workspace_version, daemon_version)

    def test_windows_script_regex_extracts_workspace_version(self) -> None:
        # Replay the script's section-scoped extraction against the repo.
        root_toml = (ROOT / "Cargo.toml").read_text()
        section = _section(root_toml, "workspace.package")
        self.assertRegex(_version(section), r"^\d+\.\d+\.\d+$")


if __name__ == "__main__":
    unittest.main()
