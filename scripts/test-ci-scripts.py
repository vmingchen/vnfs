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
                'printf "%s\\n" "${TEST_OUTPUT:-test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out}"\n'
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
        self.assertIn("--test port_helpers", calls[2])
        self.assertIn("posix test-faults", calls[2])
        self.assertFalse(any("--manifest-path" in call or "--test nfs" in call
                             or "--test smb" in call or "--doc" in call for call in calls))

    def test_smoke_dispatches_quick_and_only_quick(self):
        result = self.run_script("test-ci-local.sh", "smoke")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Running local CI job: smoke", result.stdout)
        self.assertEqual(len(self.log.read_text().splitlines()), 3)

    def test_uring_dispatch_includes_benchmark_and_nfs_free_guide(self):
        uname = self.bin / "uname"
        uname.write_text('#!/bin/sh\nprintf "Linux\\n"\n')
        uname.chmod(0o755)
        result = self.run_script("test-ci-local.sh", "uring")
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.log.read_text().splitlines()
        self.assertEqual(len(calls), 4)
        self.assertIn("test -p vfsi-uring --locked", calls[0])
        self.assertIn("--no-default-features --features uring --test uring", calls[1])
        self.assertIn("--features uring,posix --example uring_bench", calls[2])
        self.assertIn("--no-default-features --features uring --doc guides::uring", calls[3])

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

    def test_focused_selection_preserves_features_and_selects_only_one_target(self):
        for package, features in [
            ("vfsi-core", "test-faults"),
            ("vfsi-sync", "test-faults test-support"),
            ("vfsi-local", "test-faults"),
            ("vfsi-posix", "test-faults"),
            ("vfsi-uring", ""),
            ("vnfs", "posix test-faults"),
            ("vfsi-nfs", None),
            ("vfsi-smb", ""),
            ("nfsv41-sys", ""),
            ("vfsi-c", ""),
        ]:
            with self.subTest(package=package):
                result = self.run_script("test-rust.sh", "--package", package, "--lib")
                self.assertEqual(result.returncode, 0, result.stderr)
                calls = self.log.read_text().splitlines()
                self.assertEqual(len(calls), 2)
                self.assertTrue(calls[1].startswith(f"test -p {package} "))
                self.assertIn("--lib", calls[1])
                if features is None:
                    self.assertIn("--all-features", calls[1])
                elif features:
                    self.assertIn(f"--features {features}", calls[1])
                else:
                    self.assertNotIn("--features", calls[1])
                self.log.unlink()

    def test_focused_named_target_and_filter(self):
        result = self.run_script("test-rust.sh", "--package", "vnfs",
                                 "--test", "readv", "--filter", "budgets")
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.log.read_text().splitlines()
        self.assertEqual(len(calls), 2)
        self.assertIn("--test readv budgets -- --nocapture", calls[1])
        self.assertNotIn("--lib", calls[1])
        self.assertNotIn("public_api", calls[1])

    def test_focused_zero_tests_skips_and_cargo_failures_are_not_success(self):
        for output, status in [
            ("test result: ok. 0 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out", "0"),
            ("skipping NFS integration test: missing fixture\ntest result: ok. 1 passed; 0 failed", "0"),
            ("test result: FAILED. 0 passed; 1 failed", "19"),
        ]:
            with self.subTest(output=output):
                # Fail only Cargo, not the preliminary script regression check.
                cargo = self.bin / "cargo"
                cargo.write_text(f'#!/bin/sh\nprintf "%s\\n" "{output}"\nexit {status}\n')
                result = self.run_script("test-rust.sh", "--package", "vnfs", "--test", "readv")
                self.assertEqual(result.returncode, int(status) or 1)
                self.assertTrue((self.root / "timings").read_text().rstrip()
                                .endswith(f"\t{result.returncode}"))

    def test_focused_test_names_are_not_skip_diagnostics(self):
        result = self.run_script("test-rust.sh", "--package", "vnfs", "--test", "readv",
                                 TEST_OUTPUT="test skipping_existing_files ... ok\ntest result: ok. 1 passed; 0 failed")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_invalid_focused_selections_do_not_run_tests(self):
        for args in [
            ("--package",), ("--test",), ("--filter",),
            ("--package", "vnfs"), ("--lib",), ("--filter", "budgets"),
            ("--package", "unknown", "--lib"),
            ("--package", "vnfs", "--lib", "--test", "readv"),
            ("--quick", "--package", "vnfs", "--lib"),
            ("--package", "vnfs", "--package", "vfsi-core", "--lib"),
            ("--package", "vnfs", "--lib", "--filter", "--help"),
        ]:
            with self.subTest(args=args):
                result = self.run_script("test-rust.sh", *args)
                self.assertEqual(result.returncode, 2)
                self.assertFalse(self.log.exists())

    def test_live_focused_targets_require_fixture_configuration(self):
        for package, target, env in [
            ("vfsi-nfs", "nfs", dict(VFSI_NFS_SERVER="")),
            ("vnfs", "tree_builder_nfs", dict(VFSI_NFS_SERVER="", VFSI_NFS_REQUIRED="0")),
            ("vnfs", "transfer_helpers_nfs", dict(VFSI_NFS_SERVER="", VFSI_NFS_REQUIRED="0")),
            ("vfsi-smb", "smb", dict(VFSI_SMB_SERVER="", VFSI_SMB_SHARE="")),
        ]:
            with self.subTest(package=package):
                self.log.unlink(missing_ok=True)
                result = self.run_script("test-rust.sh", "--package", package,
                                         "--test", target, **env)
                self.assertEqual(result.returncode, 2)
                self.assertFalse(self.log.exists())

    def test_configured_live_targets_require_execution_and_keep_fault_features(self):
        cargo = self.bin / "cargo"
        stub = cargo.read_text().split("\n", 1)[1]
        for package, target, required in [
            ("vfsi-nfs", "nfs", "VFSI_NFS_REQUIRED"),
            ("vnfs", "tree_builder_nfs", "VFSI_NFS_REQUIRED"),
            ("vnfs", "transfer_helpers_nfs", "VFSI_NFS_REQUIRED"),
            ("vfsi-smb", "smb", "VFSI_SMB_REQUIRED"),
        ]:
            with self.subTest(package=package, target=target):
                self.log.unlink(missing_ok=True)
                cargo.write_text(f'#!/bin/sh\n[ "${{{required}:-}}" = "1" ] || exit 41\n' + stub)
                result = self.run_script(
                    "test-rust.sh", "--package", package, "--test", target,
                    VFSI_NFS_SERVER="test-server.invalid", VFSI_SMB_SERVER="test-server.invalid",
                    VFSI_SMB_SHARE="test-share", **{required: "0"})
                self.assertEqual(result.returncode, 0, result.stderr)
                calls = self.log.read_text().splitlines()
                self.assertEqual(len(calls), 2)
                self.assertIn(f"--test {target}", calls[1])
                if package == "vfsi-smb":
                    self.assertIn("--features test-faults", calls[1])
                self.log.unlink()


if __name__ == "__main__":
    unittest.main()
