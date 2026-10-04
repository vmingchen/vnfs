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



class RustQuickTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        scripts = self.root / "scripts"
        scripts.mkdir()
        for name in ("test-rust.sh", "test-ci-local.sh", "test-ci-scripts.py"):
            shutil.copy(Path(__file__).with_name(name), scripts)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.log = self.root / "calls"
        for name in ("cargo", "python3"):
            stub = self.bin / name
            stub.write_text(
                '#!/bin/sh\n'
                'printf "%s\\n" "$*" >> "$CALL_LOG"\n'
                'exit "${FAIL_STATUS:-0}"\n'
            )
            stub.chmod(0o755)
        self.env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}",
                        CALL_LOG=str(self.log),
                        VFSI_TEST_TIMINGS=str(self.root / "timings"))

    def run_script(self, name, *args, **env):
        return subprocess.run(["bash", str(self.root / "scripts" / name), *args],
                              env=dict(self.env, **env), capture_output=True, text=True)

    def test_quick_runs_fault_and_api_tests_without_adapter_or_live_jobs(self):
        result = self.run_script("test-rust.sh", "--quick")
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.log.read_text().splitlines()
        self.assertEqual(len(calls), 3)
        self.assertIn("vfsi-sync/test-faults", calls[1])
        self.assertIn("--lib", calls[1])
        self.assertIn("--test readv", calls[2])
        self.assertIn("--test application_boundary", calls[2])
        self.assertIn("dummy test-faults", calls[2])
        self.assertFalse(any("--manifest-path" in call or "--test nfs" in call
                             or "--test smb" in call or "--doc" in call for call in calls))

    def test_smoke_dispatches_quick_and_only_quick(self):
        result = self.run_script("test-ci-local.sh", "smoke")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Running local CI job: smoke", result.stdout)
        self.assertEqual(len(self.log.read_text().splitlines()), 3)

    def test_failures_propagate_and_are_recorded(self):
        result = self.run_script("test-rust.sh", "--quick", FAIL_STATUS="17")
        self.assertEqual(result.returncode, 17)
        self.assertEqual(len(self.log.read_text().splitlines()), 1)
        self.assertTrue((self.root / "timings").read_text().rstrip().endswith("\t17"))

    def test_bad_arguments_do_not_run_tests(self):
        for args in [("--unknown",), ("--quick", "extra")]:
            result = self.run_script("test-rust.sh", *args)
            self.assertEqual(result.returncode, 2)
            self.assertFalse(self.log.exists())


if __name__ == "__main__":
    unittest.main()
