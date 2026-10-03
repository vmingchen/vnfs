#!/usr/bin/env python3
"""Server-independent regression tests for the local CI runner plumbing."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class NativeFixtureTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "scripts").mkdir()
        shutil.copy(Path(__file__).with_name("test-libntirpc.sh"), self.root / "scripts")
        self.source = self.root / "source"
        self.source.mkdir()
        (self.source / "Cargo.toml").write_text('[package]\nname="libntirpc-sys"\nversion="0.0.0"\n')
        self.bin = self.root / "bin"
        self.bin.mkdir()
        cargo = self.bin / "cargo"
        cargo.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
if sys.argv[1] == "metadata":
    print(json.dumps({"packages": [{"name": "libntirpc-sys", "manifest_path": os.environ["SOURCE_MANIFEST"]}]}))
else:
    manifest = pathlib.Path(sys.argv[sys.argv.index("--manifest-path") + 1])
    assert "[workspace]" in manifest.read_text()
    marker = manifest.parent / "build-reused"
    with open(os.environ["CALL_LOG"], "a") as log:
        log.write(str(manifest) + " " + str(marker.exists()) + "\\n")
    marker.touch()
    sys.exit(int(os.environ.get("FAIL_STATUS", "0")))
''')
        cargo.chmod(0o755)
        self.log = self.root / "calls"
        self.env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}",
                        SOURCE_MANIFEST=str(self.source / "Cargo.toml"), CALL_LOG=str(self.log))

    def run_script(self, **env):
        return subprocess.run(["bash", str(self.root / "scripts/test-libntirpc.sh")],
                              env=dict(self.env, **env), capture_output=True, text=True)

    def test_reuses_fixture_without_modifying_dependency_source(self):
        for _ in range(2):
            result = self.run_script()
            self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.log.read_text().splitlines()
        self.assertEqual(calls[0].rsplit(" ", 1)[0], calls[1].rsplit(" ", 1)[0])
        self.assertTrue(calls[0].endswith("False"))
        self.assertTrue(calls[1].endswith("True"))
        self.assertNotIn("[workspace]", (self.source / "Cargo.toml").read_text())
        self.assertFalse((self.source / "build-reused").exists())

    def test_source_changes_invalidate_fixture(self):
        self.assertEqual(self.run_script().returncode, 0)
        (self.source / "patch.c").write_text("/* changed source */\n")
        self.assertEqual(self.run_script().returncode, 0)
        calls = self.log.read_text().splitlines()
        self.assertNotEqual(calls[0], calls[1])
        self.assertTrue(all(call.endswith("False") for call in calls))

    def test_failure_is_not_hidden(self):
        self.assertEqual(self.run_script(FAIL_STATUS="17").returncode, 17)


if __name__ == "__main__":
    unittest.main()
