#!/usr/bin/env python3
"""Exercise installation in isolated prefixes, without sudo or live services."""
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class Installation(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="hearth-install-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.build = self.root / "build outputs"
        self.build.mkdir()
        for binary in ("hearthd", "hearth-ircd", "hearth-admin"):
            self.binary(binary, "first")

    def binary(self, name, version):
        file = self.build / name
        file.write_text(f"#!/bin/sh\nprintf '%s\\n' '{version}'\n")
        file.chmod(0o755)

    def install(self, *args, check=True):
        return subprocess.run(["sh", str(ROOT / "scripts/install.sh"),
                               "--bin-dir", str(self.build), *map(str, args)],
                              capture_output=True, text=True, check=check)

    def test_upgrade_preserves_open_executable_and_runtime_data(self):
        prefix = self.root / "local"
        self.install("--prefix", prefix)
        executable = prefix / "bin/hearthd"
        history = prefix / "share/hearth/history.sqlite3"
        history.write_bytes(b"existing runtime data")
        with executable.open() as old:
            self.binary("hearthd", "second")
            self.install("--prefix", prefix)
            self.assertIn("first", old.read())
        self.assertEqual(subprocess.check_output([executable], text=True).strip(), "second")
        self.assertEqual(history.read_bytes(), b"existing runtime data")
        for name in ("hearthd", "hearth-ircd", "hearth-admin"):
            self.assertEqual((prefix / "bin" / name).stat().st_mode & 0o777, 0o755)
        self.assertTrue((prefix / "share/hearth/skills/hearth/SKILL.md").is_file())
        self.assertTrue((prefix / "share/hearth/deploy/systemd/hearth-ircd.service").is_file())

    def test_client_only_and_staging_with_spaces(self):
        (self.build / "hearth-ircd").unlink()
        (self.build / "hearth-admin").unlink()
        staging = self.root / "package staging"
        self.install("--prefix", "/opt/hearth test", "--destdir", staging, "--client-only")
        bindir = staging / "opt/hearth test/bin"
        self.assertEqual([p.name for p in bindir.iterdir()], ["hearthd"])

    def test_missing_binary_does_not_start_install(self):
        (self.build / "hearth-admin").unlink()
        prefix = self.root / "local"
        result = self.install("--prefix", prefix, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(prefix.exists())

    def test_rejects_relative_prefix_and_unknown_option(self):
        for arguments in (("--prefix", "relative"), ("--destdir", "relative"), ("--typo",)):
            self.assertNotEqual(self.install(*arguments, check=False).returncode, 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
