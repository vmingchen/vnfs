#!/usr/bin/env python3
"""Server-independent regression tests for the local CI runner plumbing."""
import io
import json
import os
import re
import shlex
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest


class BackendLibraryCoverageTests(unittest.TestCase):
    """Live regressions must execute with fixtures, rather than silently skip."""

    root = Path(__file__).resolve().parents[1]

    def assert_library_suite(self, script, package, features):
        commands = [shlex.split(line.strip()) for line in script.splitlines()
                    if line.strip().startswith("cargo test ")]
        library = [args for args in commands if "--lib" in args and "--ignored" not in args and
                   args[args.index("-p") + 1] == package]
        self.assertEqual(len(library), 1, script)
        args = library[0]
        self.assertEqual(args[args.index("--features") + 1], features)
        before, after = args[:args.index("--")], args[args.index("--") + 1:]
        # Any additional positional argument here filters out future regressions.
        self.assertEqual(before, ["cargo", "test", "-p", package,
                                  "--features", features, "--lib"])
        self.assertEqual(after, ["--test-threads=1"])

    def test_hosted_live_jobs_run_unfiltered_backend_libraries(self):
        workflow = (self.root / ".github/workflows/ci.yml").read_text()
        for job, step, package, features, required in [
            ("nfs-integration", "Run vfsi-nfs directly against NFS",
             "vfsi-nfs", "test-faults", "VFSI_NFS_REQUIRED: 1"),
            ("nfs-server-copy-integration", "Run vfsi-nfs directly against patched NFSv4.2",
             "vfsi-nfs", "server-copy test-faults", "VFSI_NFS_REQUIRED: 1"),
            ("smb-integration", "Run SMB conformance and integration tests",
             "vfsi-smb", "$rust_features", "VFSI_SMB_REQUIRED: 1"),
        ]:
            with self.subTest(job=job):
                # Bound the search to this job and step, including its fixture env.
                body = re.search(r"^  " + job + r":\n(.*?)(?=^  [\w-]+:|\Z)",
                                 workflow, re.M | re.S)
                self.assertIsNotNone(body, job)
                body = re.search(r"      - name: " + re.escape(step) +
                                 r"\n(.*?)(?=^      - |\Z)",
                                 body[1], re.M | re.S)
                self.assertIsNotNone(body, step)
                self.assertIn(required, body[1])
                self.assert_library_suite(body[1], package, features)

    def test_local_live_jobs_run_unfiltered_backend_libraries(self):
        runner = (self.root / "scripts/test-ci-local.sh").read_text()
        for backend in ("nfs", "smb"):
            with self.subTest(backend=backend):
                body = re.search(r"^run_" + backend + r"\(\) \{\n(.*?)^\}",
                                 runner, re.M | re.S)[1]
                self.assertIn("VFSI_" + backend.upper() + "_REQUIRED=1", body)
                self.assert_library_suite(body, "vfsi-" + backend, "test-faults")


class ArchiveVerificationTests(unittest.TestCase):
    def test_reextracted_crates_refresh_source_mtimes_for_cargos_cache(self):
        workflow = Path(__file__).resolve().parents[1] / ".github/workflows/ci.yml"
        command = next(line.strip() for line in workflow.read_text().splitlines()
                       if 'tar -' in line and '$package-$version.crate' in line)
        timestamp = 1153704088  # Cargo normalizes archive mtimes, even after edits.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            packages = root / "target/package"
            packages.mkdir(parents=True)
            archive = packages / "vfsi-core-0.1.0.crate"
            relative = "vfsi-core-0.1.0/src/error.rs"
            for contents in (b"old API", b"new API"):
                with tarfile.open(archive, "w:gz") as output:
                    entry = tarfile.TarInfo(relative)
                    entry.size = len(contents)
                    entry.mtime = timestamp
                    output.addfile(entry, io.BytesIO(contents))
                subprocess.run(["bash", "-ec", command], cwd=root, check=True,
                               env=dict(os.environ, package="vfsi-core", version="0.1.0"))
                source = packages / relative
                self.assertEqual(source.read_bytes(), contents)
                self.assertGreater(source.stat().st_mtime, timestamp,
                                   "normalized mtimes can reuse stale same-version Rust artifacts")


class LatencyHelperTests(unittest.TestCase):
    def run_helper(self, *, add_status=0, benchmark_status=0, existing=False,
                   add_signal="", benchmark_signal=""):
        script = Path(__file__).resolve().parents[1] / "ci/benchmark-large-read-pipeline.sh"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log = root / "calls"
            for name, body in {
                "tc": (
                    'printf "%s\\n" "tc $*" >> "$CALL_LOG"\n'
                    'if [ "$1 $2" = "qdisc show" ]; then\n'
                    '  if [ "$EXISTING_QDISC" = "1" ]; then echo "qdisc fq_codel 0: root";\n'
                    '  else echo "qdisc noqueue 0: root"; fi\n'
                    'elif [ "$1 $2" = "qdisc add" ]; then\n'
                    '  if [ -n "$ADD_SIGNAL" ]; then kill -s "$ADD_SIGNAL" "$PPID"; fi\n'
                    '  exit "$ADD_STATUS"\n'
                    'fi\n'
                ),
                "sudo": 'exec "$@"\n',
                "cargo": (
                    'printf "benchmark\\n" >> "$CALL_LOG"\n'
                    'if [ -n "$BENCHMARK_SIGNAL" ]; then kill -s "$BENCHMARK_SIGNAL" "$PPID"; fi\n'
                    'exit "$BENCHMARK_STATUS"\n'
                ),
            }.items():
                stub = root / name
                stub.write_text("#!/bin/sh\n" + body)
                stub.chmod(0o755)
            result = subprocess.run(
                ["bash", str(script), "127.0.0.1", "/", "/file"],
                env=dict(os.environ, PATH=f"{root}:{os.environ['PATH']}",
                         CALL_LOG=str(log), ADD_STATUS=str(add_status),
                         BENCHMARK_STATUS=str(benchmark_status),
                         ADD_SIGNAL=add_signal, BENCHMARK_SIGNAL=benchmark_signal,
                         EXISTING_QDISC=str(int(existing))),
                capture_output=True, text=True, timeout=5,
            )
            return result, log.read_text().splitlines()

    def test_failed_qdisc_creation_does_not_delete_an_unowned_qdisc(self):
        result, calls = self.run_helper(add_status=17)
        self.assertEqual(result.returncode, 17, result.stderr)
        self.assertFalse(any("qdisc del" in call for call in calls), calls)
        self.assertNotIn("benchmark", calls)

    def test_owned_qdisc_is_removed_after_benchmark_failure(self):
        result, calls = self.run_helper(benchmark_status=23)
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertIn("benchmark", calls)
        self.assertEqual(sum("qdisc del" in call for call in calls), 1)

    def test_existing_qdisc_is_refused_before_mutation(self):
        result, calls = self.run_helper(existing=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertFalse(any("qdisc add" in call or "qdisc del" in call for call in calls), calls)
        self.assertNotIn("benchmark", calls)

    def test_owned_qdisc_is_removed_after_benchmark_success(self):
        result, calls = self.run_helper()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("benchmark", calls)
        self.assertEqual(sum("qdisc del" in call for call in calls), 1)

    def test_successful_qdisc_creation_is_cleaned_up_when_interrupted(self):
        for signal, status in [("INT", 130), ("TERM", 143)]:
            with self.subTest(signal=signal):
                result, calls = self.run_helper(add_signal=signal)
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertEqual(sum("qdisc del" in call for call in calls), 1, calls)
                self.assertNotIn("benchmark", calls)

    def test_failed_qdisc_creation_keeps_unowned_qdisc_when_interrupted(self):
        for signal, status in [("INT", 130), ("TERM", 143)]:
            with self.subTest(signal=signal):
                result, calls = self.run_helper(add_status=17, add_signal=signal)
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertFalse(any("qdisc del" in call for call in calls), calls)
                self.assertNotIn("benchmark", calls)

    def test_owned_qdisc_is_removed_when_benchmark_is_interrupted(self):
        for signal, status in [("INT", 130), ("TERM", 143)]:
            with self.subTest(signal=signal):
                result, calls = self.run_helper(benchmark_signal=signal)
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertIn("benchmark", calls)
                self.assertEqual(sum("qdisc del" in call for call in calls), 1, calls)


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
        (self.root / "ci").mkdir()
        shutil.copy(Path(__file__).resolve().parents[1] / "ci/test-c-nfs.sh",
                    self.root / "ci/test-c-nfs.sh")
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

    def test_python_jobs_select_their_virtualenv_for_native_builds(self):
        template = self.root / "venv-template"
        (template / "bin").mkdir(parents=True)
        python = template / "bin/python"
        python.write_text('#!/bin/sh\nexit 0\n')
        python.chmod(0o755)
        maturin = template / "bin/maturin"
        maturin.write_text(
            '#!/bin/sh\n'
            'if [ "${VIRTUAL_ENV:-}/bin/maturin" != "$0" ]; then exit 29; fi\n'
            'printf "native-build %s\\n" "$VIRTUAL_ENV" >> "$CALL_LOG"\n'
        )
        maturin.chmod(0o755)
        bootstrap = self.bin / "python3"
        bootstrap.write_text(
            '#!/bin/sh\n'
            '[ "$1 $2" = "-m venv" ] || exit 30\n'
            'cp -R "$VENV_TEMPLATE" "$3"\n'
        )
        bootstrap.chmod(0o755)
        for selection in ("temporary", "default", "configured"):
            with self.subTest(selection=selection):
                venv = self.root / (".venv" if selection == "default" else "configured")
                if selection != "temporary":
                    shutil.copytree(template, venv)
                env = dict(VIRTUAL_ENV="/unrelated-active-environment",
                           VENV_TEMPLATE=str(template), VFSI_LOCAL_CI_VENV="")
                if selection == "configured":
                    env["VFSI_LOCAL_CI_VENV"] = str(venv)
                result = self.run_script("test-ci-local.sh", "python", **env)
                self.assertEqual(result.returncode, 0, result.stderr)
                calls = self.log.read_text().splitlines()
                self.assertEqual(len(calls), 2, calls)
                self.assertEqual(calls[0], calls[1])
                if selection != "temporary":
                    self.assertEqual(calls[0], f"native-build {venv}")
                    shutil.rmtree(venv)
                self.log.unlink()

    def test_nfs_dispatch_requires_both_retained_contracts_for_both_minor_versions(self):
        for name, body in {
            "uname": "printf 'Linux\n'\n",
            "rpcinfo": "printf '100003 4 tcp 2049 nfs\n'\n",
            "cargo": (
                'printf "%s %s\n" "${VFSI_NFS_MINOR:-none}" "$*" >> "$CALL_LOG"\n'
                'printf "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"\n'
            ),
        }.items():
            stub = self.bin / name
            stub.write_text("#!/bin/sh\n" + body)
            stub.chmod(0o755)
        venv = self.root / "venv/bin"
        venv.mkdir(parents=True)
        for name in ("python", "maturin"):
            stub = venv / name
            stub.write_text("#!/bin/sh\nexit 0\n")
            stub.chmod(0o755)
        result = self.run_script("test-ci-local.sh", "nfs",
                                 VFSI_LOCAL_CI_VENV=str(venv.parent))
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.log.read_text().splitlines()
        for minor in (1, 2):
            for test in (
                "identity::tests::live_nfs_retained_identity_survives_rename_replacement_and_reconnect",
                "directory::tests::retained_cohort_children_survive_parent_rename_with_bounded_compounds",
            ):
                self.assertEqual(calls.count(f"{minor} test -p vfsi-c --lib {test} -- --ignored --exact"), 1)

    def test_live_c_gate_rejects_empty_test_selection(self):
        result = subprocess.run(["bash", str(self.root / "ci/test-c-nfs.sh")],
                                env=dict(self.env, TEST_OUTPUT="test result: ok. 0 passed; 0 failed; 0 ignored; 1 filtered out"),
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("Required C SDK NFS test did not execute", result.stderr)

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
        self.assertIn("--no-default-features --features uring --doc uring", calls[3])

    def test_uring_rejects_empty_documentation_selection(self):
        uname = self.bin / "uname"
        uname.write_text('#!/bin/sh\nprintf "Linux\\n"\n')
        uname.chmod(0o755)
        result = self.run_script(
            "test-ci-local.sh", "uring",
            TEST_OUTPUT="test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out",
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("io_uring guide doctests did not execute", result.stderr)
        self.assertEqual(len(self.log.read_text().splitlines()), 4)

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


class BenchmarkBuildTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        import importlib.util
        cls.module_path = Path(__file__).with_name("benchmark-write-scheduler.py")
        spec = importlib.util.spec_from_file_location("write_scheduler_benchmark", cls.module_path)
        cls.benchmark = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.benchmark)

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.workspace = self.root / "workspace"
        self.workspace.mkdir()
        self.manifest = self.workspace / "Cargo.toml"
        self.manifest.write_text('[workspace]\nmembers=[]\n')
        self.environment = {"HOME": str(self.root), "CARGO_HOME": str(self.root / "cargo-home")}
        self.command = ["cargo", "build", "--release"]

    def configuration(self, environment=None):
        return self.benchmark.build_configuration(
            self.workspace, self.environment if environment is None else environment,
            self.command, "rustc test-version", "cargo test-version")

    def test_reuses_matching_build_configuration(self):
        self.benchmark.validate_reuse_build(
            {"build_configuration": self.configuration()}, self.configuration())

    def test_rejects_changed_optimization_flags(self):
        prior = {"build_configuration": self.configuration()}
        for name, value in [
            ("CARGO_PROFILE_RELEASE_OPT_LEVEL", "0"),
            ("RUSTFLAGS", "-C opt-level=0"),
            ("CARGO_ENCODED_RUSTFLAGS", "-C\x1fopt-level=0"),
            ("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS", "-C opt-level=0"),
        ]:
            with self.subTest(name=name):
                expected = self.configuration(dict(self.environment, **{name: value}))
                with self.assertRaisesRegex(RuntimeError, "configuration differs.*environment"):
                    self.benchmark.validate_reuse_build(prior, expected)

    def test_rejects_changed_profile_and_compiler_overrides(self):
        prior = {"build_configuration": self.configuration()}
        for name, value in [
            ("CARGO_PROFILE_RELEASE_LTO", "true"),
            ("CARGO_PROFILE_RELEASE_CODEGEN_UNITS", "1"),
            ("CARGO_BUILD_RUSTFLAGS", "-C target-cpu=native"),
            ("RUSTC_WRAPPER", "/compiler-wrapper"),
            ("RUSTC_WORKSPACE_WRAPPER", "/workspace-wrapper"),
            ("RUSTC", "/different-rustc"),
        ]:
            with self.subTest(name=name):
                with self.assertRaises(RuntimeError):
                    self.benchmark.validate_reuse_build(
                        prior, self.configuration(dict(self.environment, **{name: value})))

    def test_rejects_changed_workspace_profile(self):
        prior = {"build_configuration": self.configuration()}
        self.manifest.write_text('[workspace]\nmembers=[]\n[profile.release]\nopt-level=2\n')
        with self.assertRaisesRegex(RuntimeError, "workspace_profiles"):
            self.benchmark.validate_reuse_build(prior, self.configuration())

    def test_rejects_project_parent_and_home_configuration_changes(self):
        for directory in [self.workspace / ".cargo", self.root / ".cargo",
                          self.root / "cargo-home"]:
            with self.subTest(directory=directory):
                directory.mkdir(exist_ok=True)
                config = directory / "config.toml"
                prior = {"build_configuration": self.configuration()}
                config.write_text('[profile.release]\nopt-level=2\n')
                with self.assertRaisesRegex(RuntimeError, "cargo_configs"):
                    self.benchmark.validate_reuse_build(prior, self.configuration())
                config.unlink()

    def test_rejects_included_configuration_changes_and_optional_creation(self):
        directory = self.workspace / ".cargo"
        directory.mkdir()
        (directory / "config.toml").write_text(
            'include=[{path="release.toml",optional=true}]\n')
        prior = {"build_configuration": self.configuration()}
        included = directory / "release.toml"
        included.write_text('[profile.release]\nopt-level=3\n')
        with self.assertRaisesRegex(RuntimeError, "cargo_configs"):
            self.benchmark.validate_reuse_build(prior, self.configuration())
        prior = {"build_configuration": self.configuration()}
        included.write_text('[profile.release]\nopt-level=2\n')
        with self.assertRaises(RuntimeError):
            self.benchmark.validate_reuse_build(prior, self.configuration())

    def test_matches_cargo_legacy_configuration_precedence(self):
        directory = self.workspace / ".cargo"
        directory.mkdir()
        legacy = directory / "config"
        legacy.write_text('[profile.release]\nopt-level=3\n')
        toml = directory / "config.toml"
        toml.write_text('[profile.release]\nopt-level=0\n')
        prior = {"build_configuration": self.configuration()}
        toml.write_text('[profile.release]\nopt-level=2\n')
        self.benchmark.validate_reuse_build(prior, self.configuration())
        legacy.unlink()
        with self.assertRaises(RuntimeError):
            self.benchmark.validate_reuse_build(prior, self.configuration())

    def test_rejects_legacy_results_without_build_configuration(self):
        with self.assertRaisesRegex(RuntimeError, "no verified build configuration.*rebuild"):
            self.benchmark.validate_reuse_build({"binary_sha256": "hash"}, self.configuration())

    def test_rejects_changed_tool_versions_and_build_command(self):
        prior = {"build_configuration": self.configuration()}
        for key, value in [("rustc", "other compiler"), ("cargo", "other cargo"),
                           ("command", ["cargo", "build"])]:
            with self.subTest(key=key):
                expected = dict(self.configuration(), **{key: value})
                with self.assertRaises(RuntimeError):
                    self.benchmark.validate_reuse_build(prior, expected)

    def test_keeps_credentials_and_cache_locations_out_of_configuration(self):
        environment = dict(self.environment, CARGO_REGISTRY_TOKEN="placeholder-token",
                           CARGO_TARGET_DIR="/different-cache")
        self.assertEqual(self.configuration(environment), self.configuration())
        directory = self.workspace / ".cargo"
        directory.mkdir()
        (directory / "config.toml").write_text('[registry]\ntoken="placeholder-token"\n')
        self.assertNotIn("placeholder-token", json.dumps(self.configuration(environment)))

    def test_reuse_checks_survive_python_optimization(self):
        import sys
        code = f"""
import importlib.util
spec = importlib.util.spec_from_file_location('benchmark', {str(self.module_path)!r})
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)
try:
    benchmark.validate_reuse_build(
        {{'build_configuration': {{'environment': {{}}}}}},
        {{'environment': {{'RUSTFLAGS': '-C opt-level=0'}}}})
except RuntimeError:
    raise SystemExit(0)
raise SystemExit(1)
"""
        result = subprocess.run([sys.executable, "-O", "-c", code], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_reads_effective_profiles_from_cached_artifacts(self):
        profile = {"opt_level": "3", "debug_assertions": False, "overflow_checks": False,
                   "debuginfo": 0, "test": False}
        messages = [json.dumps({"reason": "compiler-artifact", "fresh": True,
                               "target": {"name": name, "kind": [kind]}, "profile": profile})
                    for name, kind in [("vfsi_sync", "lib"), ("write_schedule_benchmark", "example")]]
        profiles = self.benchmark.compiler_profiles("Cargo progress\n" + "\n".join(messages))
        self.assertEqual(profiles["vfsi_sync"]["opt_level"], "3")
        self.assertEqual(profiles["write_schedule_benchmark"], profile)

    def test_selects_cargo_executable_for_a_configured_target(self):
        executable = "/cache/aarch64-unknown-linux-gnu/release/examples/write_schedule_benchmark"
        message = json.dumps({"reason": "compiler-artifact", "fresh": True,
                              "target": {"name": "write_schedule_benchmark"},
                              "executable": executable})
        self.assertEqual(self.benchmark.benchmark_executable(message), Path(executable))
        with self.assertRaisesRegex(RuntimeError, "missing the benchmark executable"):
            self.benchmark.benchmark_executable('{"reason":"build-finished","success":true}')

    def test_rejects_missing_compiler_profile_evidence(self):
        with self.assertRaisesRegex(RuntimeError, "missing scheduler/example"):
            self.benchmark.compiler_profiles('{"reason":"build-finished","success":true}\n')


class PairedBenchmarkTests(unittest.TestCase):
    def setUp(self):
        import importlib.util
        import sys
        self.script = Path(__file__).with_name('benchmark-paired.py')
        spec = importlib.util.spec_from_file_location('paired', self.script)
        self.paired = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.paired)
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.fixture = self.root / 'fixture with spaces'
        self.fixture.mkdir()
        (self.fixture / 'input').write_bytes(b'unchanged')
        self.output = self.root / 'results'
        self.base = [sys.executable, str(self.script), '--fixture', str(self.fixture),
                     '--output', str(self.output), '--pairs', '3', '--cache-state', 'warm']

    def run_cli(self, before, after, *extra):
        return subprocess.run([*self.base, '--baseline', before, '--candidate', after, *extra],
                              capture_output=True, text=True, timeout=10)

    def test_timing_uses_blocking_wait_without_timeout_polling(self):
        from unittest.mock import patch
        original = self.paired.subprocess.Popen.wait
        calls = []
        def wait(process, timeout=None):
            calls.append(timeout)
            return original(process, timeout=timeout)
        with patch.object(self.paired.subprocess.Popen, 'wait', wait):
            result = self.paired.run_command(['true'], self.root / 'timing', 1)
        self.assertTrue(calls)
        self.assertEqual(calls, [None])
        self.assertGreater(result['elapsed_ns'], 0)

    def test_abba_pairs_preserve_fixture_outputs_and_refuse_overwrite(self):
        command = 'printf identical'
        result = self.run_cli(command, command, '--check-output')
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads((self.output / 'comparison.json').read_text())
        self.assertEqual([s['label'] for s in report['samples']],
                         ['baseline', 'candidate', 'candidate', 'baseline', 'baseline', 'candidate'])
        self.assertEqual((self.fixture / 'input').read_bytes(), b'unchanged')
        self.assertTrue(report['fixture_objects'])
        original = (self.output / 'comparison.json').read_bytes()
        self.assertNotEqual(self.run_cli(command, command).returncode, 0)
        self.assertEqual((self.output / 'comparison.json').read_bytes(), original)

    def test_rejects_output_mismatch_failed_commands_and_timeout(self):
        for candidate, extra in [('printf different', ['--check-output']),
                                 ('false', []), ('sleep 30', ['--timeout', '0.05'])]:
            with self.subTest(candidate=candidate):
                self.output = self.root / ('results-' + str(len(list(self.root.iterdir()))))
                self.base[self.base.index('--output') + 1] = str(self.output)
                result = self.run_cli('printf identical', candidate, *extra)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse((self.output / 'comparison.json').exists())

    def test_paths_are_expanded_as_arguments_and_inode_changes_are_detected(self):
        tokens = self.paired.command('printf "%s" {fixture}', {'fixture': str(self.fixture)})
        self.assertEqual(tokens[-1], str(self.fixture))
        before = self.paired.fixture_objects(self.fixture)
        (self.fixture / 'input').rename(self.fixture / 'old')
        (self.fixture / 'input').write_bytes(b'unchanged')
        self.assertNotEqual(before, self.paired.fixture_objects(self.fixture))

    def test_structured_comparisons_require_matching_objects_and_geometry(self):
        case = {'backend': 'uring', 'case': 'read', 'rounds': 20, 'bytes_per_call': 8,
                'requests_per_call': 2, 'request_bytes': 4, 'files': 2, 'cache': 'warm',
                'allocation': 'owned', 'p50_ns': 100}
        before = {'environment': {'fixture_objects': [1]}, 'cases': [case]}
        after = json.loads(json.dumps(before))
        after['cases'][0]['p50_ns'] = 50
        self.assertEqual(self.paired.metrics(before, after), {'uring/read': 2})
        after['cases'][0]['rounds'] = 19
        with self.assertRaisesRegex(RuntimeError, 'geometry'):
            self.paired.metrics(before, after)
        after = json.loads(json.dumps(before))
        after['environment']['fixture_objects'] = [2]
        with self.assertRaisesRegex(RuntimeError, 'objects differ'):
            self.paired.metrics(before, after)
        with self.assertRaisesRegex(RuntimeError, 'only one'):
            self.paired.metrics(before, None)
        self.assertEqual(self.paired.comparison([2, 2, 2])['bootstrap_95_percent_interval'], [2, 2])


class EcosystemBaselineTests(unittest.TestCase):
    def setUp(self):
        import importlib.util
        self.repo = Path(__file__).resolve().parents[1]
        spec = importlib.util.spec_from_file_location('ecosystem', self.repo / 'ci/check-ecosystem.py')
        self.checker = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.checker)
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        paths = [*self.repo.glob('crates/*/Cargo.toml'), self.repo / 'bindings/c/Cargo.toml',
                 *self.repo.glob('adapters/*/pyproject.toml'), *self.repo.glob('ecosystem/*.toml'),
                 *self.repo.glob('ci/ports/*'), self.repo / 'ci/nfs-ganesha/Dockerfile']
        for path in paths:
            if path.is_file():
                target = self.root / path.relative_to(self.repo)
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy(path, target)
        self.checker.ROOT = self.root
        self.checker.REGISTRY = self.root / 'ecosystem/ports.toml'

    def test_current_baseline_validates(self):
        self.checker.main()

    def test_missing_port_and_stale_manifest_pin_are_rejected(self):
        registry = self.checker.REGISTRY
        original = registry.read_text()
        registry.write_text(original.replace('id = "coreutils"', 'id = "other"'))
        with self.assertRaisesRegex(SystemExit, 'all four'):
            self.checker.main()
        registry.write_text(original)
        baseline = self.root / 'ecosystem/versions.toml'
        baseline.write_text(baseline.read_text().replace('coreutils = "', 'coreutils = "0', 1))
        with self.assertRaisesRegex(SystemExit, 'port revisions'):
            self.checker.main()

    def test_published_snapshot_can_lag_development_but_not_claim_future_versions(self):
        import re
        manifest = self.root / 'crates/vnfs/Cargo.toml'
        manifest.write_text(re.sub(r'(?m)^version = "[^"]+"', 'version = "99.0.0"', manifest.read_text(), count=1))
        self.checker.main()
        baseline = self.root / 'ecosystem/versions.toml'
        baseline.write_text(re.sub(r'(vnfs = \{ version = ")[^"]+', r'\g<1>100.0.0', baseline.read_text(), count=1))
        with self.assertRaisesRegex(SystemExit, 'exceeds source'):
            self.checker.main()

    def test_port_dependency_retarget_preserves_features_and_rejects_ambiguity(self):
        import runpy
        retarget = runpy.run_path(str(self.repo / 'ci/ports/check-rust.py'))['use_development_vnfs']
        manifest = self.root / 'Cargo.toml'
        manifest.write_text('vnfs = { version = "0.0.26", features = ["posix"], optional = true }\n')
        retarget(manifest, self.root)
        import tomllib
        dep = tomllib.loads(manifest.read_text())['vnfs']
        self.assertEqual(dep, {'path': str(self.root / 'crates/vnfs'), 'features': ['posix'], 'optional': True})
        with self.assertRaisesRegex(ValueError, 'exactly one'):
            retarget(manifest, self.root)


class PortWorkflowTriggerTests(unittest.TestCase):
    def test_relevant_pushes_are_not_limited_to_main(self):
        import fnmatch
        import re
        workflow = (Path(__file__).resolve().parents[1] / ".github/workflows/ports.yml").read_text()
        push = re.search(r"(?ms)^  push:\n(.*?)(?=^  [a-z_]+:)", workflow).group(1)
        # Keep tag pushes excluded: GitHub does not apply path filters to tags.
        branches = json.loads(re.search(r"(?m)^    branches: (.+)$", push).group(1))
        self.assertEqual(branches, ["**"])
        self.assertNotRegex(push, r"(?m)^    (?:branches-ignore|tags|tags-ignore):")
        paths = [json.loads(line.strip()[2:]) for line in push.splitlines()
                 if line.strip().startswith("- ")]
        for path in ("Cargo.toml", "Cargo.lock", "crates/vfsi-sync/src/client/mod.rs",
                     "bindings/c/include/vfsi_dynamic.h", "ecosystem/ports.toml",
                     "ci/ports/check-c-abi.sh", "ci/check-promotion.py",
                     "scripts/test-ci-scripts.py", ".github/workflows/ports.yml",
                     ".github/workflows/ci.yml", ".github/workflows/promote-vfsi.yml"):
            with self.subTest(path=path):
                self.assertTrue(any(fnmatch.fnmatchcase(path, pattern) for pattern in paths))
        self.assertFalse(any(fnmatch.fnmatchcase("docs/unrelated.md", pattern) for pattern in paths))


class PromotionGateTests(unittest.TestCase):
    def setUp(self):
        import runpy
        self.verify = runpy.run_path(str(Path(__file__).resolve().parents[1] / "ci/check-promotion.py"))["verify"]
        self.sha = "a" * 40

    def row(self, **overrides):
        row = dict(databaseId=1, headSha=self.sha, headBranch="main", status="completed",
                   conclusion="success", event="push", createdAt="2026-11-02T00:00:00Z", url="https://example.invalid/run/1")
        return dict(row, **overrides)

    def jobs(self):
        return [dict(name=name, status="completed", conclusion="success")
                for name in ("registry", "abi-v3", "ports (git)", "ports (rsync)",
                             "ports (coreutils)", "ports (findutils)")]

    def check(self, ci, ports, jobs=None):
        from unittest.mock import patch
        replies = [json.dumps(ci), json.dumps(ports),
                   json.dumps({"jobs": self.jobs() if jobs is None else jobs})]
        with patch("subprocess.check_output", side_effect=replies):
            return self.verify("vmingchen/vnfs", self.sha)

    def test_both_exact_main_runs_are_required(self):
        self.assertEqual(self.check([self.row()], [self.row()])["commit"], self.sha)
        for bad in ([], [self.row(headSha="b"*40)], [self.row(headBranch="other")],
                    [self.row(event="pull_request")], [self.row(conclusion="failure")],
                    [self.row(status="in_progress", conclusion="")]):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                self.check([self.row()], bad)
            with self.subTest(ci=bad), self.assertRaises(ValueError):
                self.check(bad, [self.row()])

    def test_new_pending_or_failed_run_does_not_reuse_older_success(self):
        for state in (dict(status="in_progress", conclusion=""), dict(conclusion="failure")):
            with self.subTest(state=state), self.assertRaises(ValueError):
                self.check([self.row()], [self.row(), self.row(databaseId=2, **state)])
            with self.subTest(ci=state), self.assertRaises(ValueError):
                self.check([self.row(), self.row(databaseId=2, **state)], [self.row()])
        self.check([self.row()], [self.row(event="workflow_dispatch")])
        with self.assertRaises(ValueError):
            self.check([self.row(event="workflow_dispatch")], [self.row()])

    def test_green_workflow_cannot_omit_any_port_or_abi_gate(self):
        for required in self.jobs():
            with self.subTest(job=required["name"]), self.assertRaises(ValueError):
                self.check([self.row()], [self.row()],
                           [job for job in self.jobs() if job["name"] != required["name"]])
        with self.assertRaises(ValueError):
            self.check([self.row()], [self.row()], [])

    def test_required_jobs_must_complete_successfully(self):
        for required in self.jobs():
            for state in (dict(conclusion="skipped"), dict(conclusion="failure"),
                          dict(conclusion="cancelled"), dict(conclusion="neutral"),
                          dict(conclusion="timed_out"),
                          dict(status="in_progress", conclusion="")):
                jobs = [dict(job, **state) if job["name"] == required["name"] else job
                        for job in self.jobs()]
                with self.subTest(job=required["name"], state=state), self.assertRaises(ValueError):
                    self.check([self.row()], [self.row()], jobs)

    def test_duplicate_job_evidence_is_rejected(self):
        for required in self.jobs():
            with self.subTest(job=required["name"]), self.assertRaises(ValueError):
                self.check([self.row()], [self.row()], [*self.jobs(), required])

    def test_job_evidence_comes_from_the_latest_eligible_port_run(self):
        from unittest.mock import patch
        replies = [json.dumps([self.row()]),
                   json.dumps([self.row(), self.row(databaseId=2),
                               self.row(databaseId=3, headSha="b" * 40)]),
                   json.dumps({"jobs": [*self.jobs(),
                                        dict(name="optional", status="completed",
                                             conclusion="skipped")]})]
        with patch("subprocess.check_output", side_effect=replies) as query:
            result = self.verify("vmingchen/vnfs", self.sha)
        self.assertEqual(result["checks"]["ports.yml"]["databaseId"], 2)
        self.assertEqual(query.call_args.args[0],
                         ["gh", "run", "view", "2", "--repo", "vmingchen/vnfs",
                          "--json", "jobs"])

    def test_job_query_failure_cannot_promote(self):
        from unittest.mock import patch
        with patch("subprocess.check_output",
                   side_effect=[json.dumps([self.row()]), json.dumps([self.row()]),
                                subprocess.CalledProcessError(1, ["gh", "run", "view"])]):
            with self.assertRaises(subprocess.CalledProcessError):
                self.verify("vmingchen/vnfs", self.sha)

    def test_invalid_commit_fails_before_running_gh(self):
        from unittest.mock import patch
        with patch("subprocess.check_output") as query, self.assertRaises(ValueError):
            self.verify("vmingchen/vnfs", "main")
        query.assert_not_called()



class CSDKTests(unittest.TestCase):
    def test_packaged_sdk_recipe(self):
        subprocess.run([sys.executable, str(Path(__file__).resolve().parents[1] / "bindings/c/tests/test_build_sdk.py")], check=True)

    @unittest.skipUnless(sys.platform.startswith("linux") or sys.platform == "darwin",
                         "native C headers require Linux or macOS")
    def test_native_stat_projection(self):
        subprocess.run([str(Path(__file__).resolve().parents[1] / "ci/ports/check-c-headers.sh")],
                       check=True)

if __name__ == "__main__":
    unittest.main()
