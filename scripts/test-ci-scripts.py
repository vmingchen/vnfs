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


class OutcomeConformanceTests(unittest.TestCase):
    root = Path(__file__).resolve().parents[1]

    def test_packaged_consumer_and_real_legacy_fallback_are_required(self):
        workflow = (self.root / ".github/workflows/ci.yml").read_text()
        self.assertIn('--include "$archive/include" --consumer-source "$archive/tests/io-outcomes.c"', workflow)
        runner = (self.root / "ci/test-c-outcomes.sh").read_text()
        self.assertIn("https://static.crates.io/crates/vfsi-c/vfsi-c-0.3.11.crate", runner)
        self.assertIn("sha256sum -c -", runner)
        self.assertIn("--registry-only", runner)
        self.assertIn('"$legacy_library" legacy "$work/files"', runner)
        self.assertIn('"$legacy_library" posix "$work/files"', runner)
        self.assertIn('"$work/no-such-library.so" posix "$work/files"', runner)

    def test_both_live_versions_require_all_three_adapters_and_kernel_rows(self):
        workflow = (self.root / ".github/workflows/ci.yml").read_text()
        body = re.search(r"^  nfs-integration:\n(.*?)(?=^  [\w-]+:|\Z)",
                         workflow, re.M | re.S)[1]
        self.assertIn("minor-version: [1, 2]", body)
        self.assertIn("ci/test-c-outcomes.sh", body)
        self.assertIn("--adapter rust", body)
        self.assertIn("--adapter python", body)
        self.assertIn("cargo build -p vfsi-nfs --example io-outcome-consumer", body)
        self.assertNotIn("continue-on-error", body)
        self.assertIn("run: ci/test-uring-outcomes.sh", workflow)


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



class RustRunnerTests(unittest.TestCase):
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
        native = scripts / "test-libntirpc.sh"
        native.write_text('#!/bin/sh\nprintf "native-tests\\n" >> "$CALL_LOG"\nexit "${FAIL_STATUS:-0}"\n')
        native.chmod(0o755)
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

    def test_default_and_fast_aliases_run_only_native_free_unit_tests(self):
        for args in [(), ("--fast",), ("--quick",)]:
            with self.subTest(args=args):
                self.log.unlink(missing_ok=True)
                result = self.run_script("test-rust.sh", *args)
                self.assertEqual(result.returncode, 0, result.stderr)
                calls = self.log.read_text().splitlines()
                self.assertEqual(len(calls), 1, calls)
                args = shlex.split(calls[0])
                packages = [args[i + 1] for i, a in enumerate(args) if a == "-p"]
                self.assertEqual(packages, ["vfsi-core", "vfsi-sync", "vfsi-local", "vfsi-posix"])
                self.assertIn("--lib", args)
                self.assertIn("vfsi-sync/test-faults", calls[0])
                self.assertIn("vfsi-local/test-faults", calls[0])
                self.assertFalse(any(a in args for a in ("--test", "--doc", "--manifest-path")))
                self.assertNotIn("clippy", args)
                self.assertNotIn("fmt", args)
                skipped = [args[i + 1] for i, arg in enumerate(args) if arg == "--skip"]
                self.assertEqual(skipped, [
                    "contract_tests::tests::recursive_copy_uses_bounded_directory_pages_and_native_file_batches",
                    "contract_tests::tests::deep_recursive_operations_use_bounded_call_stack",
                ])

    def test_local_default_and_lightweight_aliases_only_dispatch_fast(self):
        for args in [(), ("fast",), ("smoke",), ("quick",), ("fast", "quick")]:
            with self.subTest(args=args):
                self.log.unlink(missing_ok=True)
                result = self.run_script("test-ci-local.sh", *args)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("Running local CI job: fast", result.stdout)
                self.assertRegex(result.stdout, r"==> fast completed in \d+\.\d{3} seconds")
                self.assertEqual(len(self.log.read_text().splitlines()), 1)

    def assert_full_coverage(self, calls):
        self.assertFalse(any("--skip" in call for call in calls))
        self.assertEqual(calls.count("scripts/test-ci-scripts.py"), 1)
        self.assertEqual(calls.count("native-tests"), 1)
        common = next(call for call in calls if call.startswith("test -p vfsi-core "))
        for package in ("vfsi-core", "vfsi-sync", "vfsi-local", "vfsi-posix"):
            self.assertIn("-p " + package, common)
        self.assertNotIn("--lib", shlex.split(common))
        for feature in ("vfsi-core/test-faults", "vfsi-sync/test-faults",
                        "vfsi-sync/test-support", "vfsi-local/test-faults"):
            self.assertIn(feature, common)
        for adapter in ("vfsi-python", "nfs4fs", "vsmb"):
            self.assertEqual(sum("--manifest-path adapters/" + adapter + "/Cargo.toml" in call
                                 and call.startswith("test ") for call in calls), 1)
        for command in ("-p vfsi-nfs --lib --all-features", "-p vfsi-smb --lib",
                        "-p nfsv41-sys", "-p vfsi-c --lib"):
            self.assertTrue(any(command in call for call in calls), command)
        facade = [shlex.split(call) for call in calls if call.startswith("test -p vnfs ")]
        self.assertEqual(len(facade), 2, "One shared feature build plus doctests")
        targets = [args[i + 1] for args in facade
                   for i, arg in enumerate(args) if arg == "--test"]
        self.assertEqual(set(targets), {"posix_backend", "public_api", "application_boundary",
                         "tree_builder", "transfer_helpers", "canonical_examples", "readv",
                         "client_vectors", "directory_pages", "port_helpers", "posix"})
        self.assertTrue(any("--lib" in args for args in facade))
        self.assertTrue(any("--doc" in args for args in facade))
        self.assertTrue(all(args[args.index("--features") + 1:args.index("--features") + 3] == ["posix", "test-faults"]
                            for args in facade))

    def test_fast_stress_filters_reference_existing_unignored_tests(self):
        root = Path(__file__).resolve().parents[1]
        runner = (root / "scripts/test-rust.sh").read_text()
        local = (root / "crates/vfsi-local/src/contract_tests.rs").read_text()
        names = [line.split("--skip ", 1)[1].split()[0].removeprefix("contract_tests::tests::")
                 for line in runner.splitlines() if "--skip " in line]
        self.assertEqual(len(names), 2)
        for name in names:
            before = local[:local.index("    fn " + name + "(")].rstrip()
            self.assertTrue(before.endswith("#[test]"), name)

    def test_full_retains_backends_faults_docs_ffi_and_all_detached_adapters(self):
        result = self.run_script("test-rust.sh", "--full")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_full_coverage(self.log.read_text().splitlines())

    def test_explicit_local_rust_dispatches_full_without_duplicate_fast_run(self):
        result = self.run_script("test-ci-local.sh", "fast", "rust")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Running local CI job: rust", result.stdout)
        self.assertNotIn("Running local CI job: fast", result.stdout)
        self.assert_full_coverage(self.log.read_text().splitlines())

    def test_local_full_retains_python_and_both_live_backend_jobs(self):
        rpcinfo = self.bin / "rpcinfo"
        rpcinfo.write_text('#!/bin/sh\nprintf "100003 4 tcp 2049 nfs\\n"\n')
        rpcinfo.chmod(0o755)
        venv = self.root / "configured/bin"
        venv.mkdir(parents=True)
        for name in ("python", "maturin"):
            stub = venv / name
            stub.write_text(f'#!/bin/sh\nprintf "{name} %s\\n" "$*" >> "$CALL_LOG"\n')
            stub.chmod(0o755)
        result = self.run_script(
            "test-ci-local.sh", "full", VFSI_LOCAL_CI_VENV=str(venv.parent),
            python_bin="inherited", smb_python_ready="inherited",
            VFSI_NFS_SERVER="test-server.invalid", VFSI_SMB_SERVER="test-server.invalid",
            VFSI_SMB_SHARE="test-share", VFSI_SMB_LOCAL_ROOT=str(self.root),
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        jobs = re.findall(r"Running local CI job: (\w+)", result.stdout)
        self.assertEqual(jobs, ["check", "rust", "python", "nfs", "smb"])
        calls = self.log.read_text().splitlines()
        self.assertEqual(calls.count("native-tests"), 1)
        for package in ("vfsi-fsspec", "vsmbfs"):
            self.assertEqual(
                calls.count(f"python -m pip install --no-deps -e adapters/{package} -q"),
                1,
            )
        for package in ("nfs4fs", "vsmb"):
            self.assertEqual(
                calls.count(f"maturin develop --manifest-path adapters/{package}/Cargo.toml --locked"),
                1,
            )
        self.assertTrue(any("pytest adapters/nfs4fs/tests" in call for call in calls))
        self.assertTrue(any("pytest adapters/vsmb/tests adapters/vsmbfs/tests" in call
                            for call in calls))
        self.assertEqual(sum("--test nfs" in call and "-p vfsi-nfs" in call
                             for call in calls), 2)
        self.assertTrue(any("-p vfsi-smb --features test-faults --test smb" in call
                            for call in calls))

    def test_hosted_ci_requires_explicit_full_mode(self):
        workflow = (Path(__file__).resolve().parents[1] / ".github/workflows/ci.yml").read_text()
        calls = [line.strip().split("run: ", 1)[1] for line in workflow.splitlines()
                 if "run: ./scripts/test-rust.sh" in line]
        self.assertEqual(calls, ["./scripts/test-rust.sh --full"])

    def test_full_stops_at_a_failed_backend_and_records_subsecond_timings(self):
        cargo = self.bin / "cargo"
        cargo.write_text(
            '#!/bin/sh\nprintf "%s\\n" "$*" >> "$CALL_LOG"\n'
            'case "$*" in "test -p vfsi-nfs "*) exit 23 ;; esac\n'
            'printf "test result: ok. 1 passed; 0 failed\\n"\n'
        )
        result = self.run_script("test-rust.sh", "--full")
        self.assertEqual(result.returncode, 23, result.stderr)
        calls = self.log.read_text().splitlines()
        self.assertTrue(calls[-1].startswith("test -p vfsi-nfs "))
        self.assertFalse(any("vfsi-smb" in call or "--manifest-path" in call for call in calls))
        rows = (self.root / "timings").read_text().splitlines()
        self.assertEqual(len(rows), 4)
        for row in rows[1:]:
            command, seconds, status = row.split("\t")
            self.assertRegex(seconds, r"^\d+\.\d{3}$")
            self.assertGreaterEqual(float(seconds), 0)
        self.assertEqual(rows[-1].rsplit("\t", 1)[1], "23")

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
                'printf "%s\\n" "facade-endpoint ${VNFS_TEST_HOST:-unset}" >> "$CALL_LOG"\n'
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
        for server, override, endpoint in [
            ("test-server.invalid", "", "test-server.invalid:2049"),
            ("127.0.0.1:12345", "", "127.0.0.1:12345"),
            ("::1", "", "[::1]:2049"),
            ("[::1]", "", "[::1]:2049"),
            ("[::1]:12345", "", "[::1]:12345"),
            ("test-server.invalid", "explicit.invalid:12345", "explicit.invalid:12345"),
        ]:
            with self.subTest(server=server, override=override):
                if self.log.exists():
                    self.log.unlink()
                result = self.run_script(
                    "test-ci-local.sh", "nfs", VFSI_LOCAL_CI_VENV=str(venv.parent),
                    VFSI_NFS_SERVER=server, VNFS_TEST_HOST=override,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                calls = self.log.read_text().splitlines()
                endpoints = {line for line in calls if line.startswith("facade-endpoint ")}
                self.assertEqual(endpoints, {f"facade-endpoint {endpoint}"})
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
                self.assertEqual(len(calls), 1)
                self.assertTrue(calls[0].startswith(f"test -p {package} "))
                self.assertIn("--lib", calls[0])
                if features is None:
                    self.assertIn("--all-features", calls[0])
                elif features:
                    self.assertIn(f"--features {features}", calls[0])
                else:
                    self.assertNotIn("--features", calls[0])
                self.log.unlink()

    def test_focused_named_target_and_filter(self):
        result = self.run_script("test-rust.sh", "--package", "vnfs",
                                 "--test", "readv", "--filter", "budgets")
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.log.read_text().splitlines()
        self.assertEqual(len(calls), 1)
        self.assertIn("--test readv budgets -- --nocapture", calls[0])
        self.assertNotIn("--lib", calls[0])
        self.assertNotIn("public_api", calls[0])

    def test_focused_public_uring_target_enables_the_backend(self):
        result = self.run_script("test-rust.sh", "--package", "vnfs", "--test", "uring")
        self.assertEqual(result.returncode, 0, result.stderr)
        command = self.log.read_text().strip()
        self.assertIn("--features uring", command)
        self.assertIn("--test uring", command)

    def test_focused_zero_tests_skips_and_cargo_failures_are_not_success(self):
        for output, status in [
            ("test result: ok. 0 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out", "0"),
            ("skipping NFS integration test: missing fixture\ntest result: ok. 1 passed; 0 failed", "0"),
            ("test result: FAILED. 0 passed; 1 failed", "19"),
        ]:
            with self.subTest(output=output):
                # Verify Cargo errors and malformed/empty selections propagate unchanged.
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
            ("--fast", "--full"), ("--full", "--fast"), ("--fast", "--quick"),
            ("--full", "--package", "vnfs", "--lib"),
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
            *(("vfsi-nfs", target, dict(VFSI_NFS_SERVER="")) for target in (
                "prr_exclusive_symlinks", "prr_rust20_round11",
                "prr_rust20_round12", "prr_session_identity",
            )),
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
            *(("vfsi-nfs", target, "VFSI_NFS_REQUIRED") for target in (
                "prr_exclusive_symlinks", "prr_rust20_round11",
                "prr_rust20_round12", "prr_session_identity",
            )),
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
                self.assertEqual(len(calls), 1)
                self.assertIn(f"--test {target}", calls[0])
                if package == "vfsi-smb":
                    self.assertIn("--features test-faults", calls[0])
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


class BenchmarkPreflightTests(unittest.TestCase):
    def setUp(self):
        self.repo = Path(__file__).resolve().parents[1]
        self.temp = tempfile.TemporaryDirectory(dir="/tmp")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_nonfinite_durations_fail_before_outputs_or_external_commands(self):
        import importlib.util
        from unittest.mock import patch
        for relative, option, required in [
            ("scripts/benchmark-write-scheduler.py", "--target-ms", []),
            ("benchmarks/error-representations/run.py", "--seconds",
             ["--timing", "unused", "--allocations", "unused"]),
        ]:
            spec = importlib.util.spec_from_file_location("duration_preflight", self.repo / relative)
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            for index, value in enumerate(["nan", "inf", "-inf", "0", "-1"]):
                with self.subTest(script=relative, value=value):
                    output = self.root / (Path(relative).parent.name + str(index))
                    with patch.object(sys, "argv", ["benchmark", *required, f"{option}={value}", "--output", str(output)]), \
                         patch.object(module.subprocess, "check_output", side_effect=AssertionError("preflight dispatched a command")), \
                         patch.object(module.subprocess, "run", side_effect=AssertionError("preflight dispatched a command")):
                        with self.assertRaises(SystemExit) as caught:
                            module.main()
                    self.assertEqual(caught.exception.code, 2)
                    self.assertFalse(output.exists())

    def test_allocation_controls_do_not_require_timing_binaries(self):
        import importlib.util
        from unittest.mock import patch

        path = self.repo / "benchmarks/error-integration/run.py"
        spec = importlib.util.spec_from_file_location("allocation_controls", path)
        runner = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(runner)
        output = self.root / "controls.json"
        for backend in ("nfs41", "nfs42", "smb"):
            (self.root / f"{backend}.root").write_text(str(self.root))
        (self.root / "final-source.patch").write_text("test diff")
        for variant in ("baseline", "final"):
            (self.root / f"{variant}.revision").write_text("test revision")
            (self.root / f"{variant}-allocations").write_bytes(b"test allocations")
            source = self.root / f"{variant}-source"
            for relative in (
                "benchmarks/error-integration/Cargo.toml",
                "benchmarks/error-integration/Cargo.lock",
                "benchmarks/error-integration/src/main.rs",
                "crates/vfsi-nfs/src/nfs/io.rs",
            ):
                file = source / relative
                file.parent.mkdir(parents=True, exist_ok=True)
                file.write_text("test source")

        def checked(args, **kwargs):
            if args[0] != "taskset":
                return "test toolchain"
            binary, backend, _, count, _ = args[3:]
            self.assertTrue(binary.name.endswith("-allocations"))
            self.assertTrue(binary.is_file())
            return json.dumps({
                "iterations": count,
                "allocation_instrumented": True,
                "closed_handles": 64,
                "ring": {
                    "sqes": 64 * count, "cqes": 64 * count, "waves": count,
                    "retained_descriptors": 0, "retained_buffer_bytes": 0,
                } if backend == "uring" else None,
                "nfs": {"rpcs": 1} if backend.startswith("nfs") else None,
            })

        with patch.object(runner, "checked", checked):
            runner.main([
                "--work", str(self.root), "--output", str(output),
                "--controls-only", "--pairs", "0",
            ])
        result = json.loads(output.read_text())
        self.assertTrue(result["allocation_controls"])
        self.assertFalse(result["timings"])
        self.assertEqual(result["pairs"], 0)
        self.assertTrue(all(set(hashes) == {"allocations"}
                            for hashes in result["binary_sha256"].values()))

    def test_assertion_based_evidence_tools_refuse_optimized_python_before_effects(self):
        for relative, entry, takes_args in [
            ("benchmarks/du/compare.py", "main", True),
            ("benchmarks/error-integration/trace.py", "main", False),
            ("scripts/benchmark-write-scheduler.py", "main", False),
            ("benchmarks/rsync/adoption.py", "prepare", True),
        ]:
            with self.subTest(script=relative):
                path = self.repo / relative
                code = f"""
import importlib.util, pathlib, subprocess, sys
from types import SimpleNamespace
from unittest.mock import patch
path = pathlib.Path({str(path)!r})
sys.path.insert(0, str(path.parent))
spec = importlib.util.spec_from_file_location('optimized_preflight', path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
output = pathlib.Path({str(self.root / "no-effects")!r})
args = SimpleNamespace(pairs=3, captures=1, output=output, target_dir=output, checkout=output)
sys.argv = ['benchmark', '--work', str(output)] if 'trace.py' == path.name else ['benchmark', '--output', str(output)]
with (
    patch.object(pathlib.Path, 'mkdir', side_effect=AssertionError('created output')),
    patch.object(subprocess, 'run', side_effect=AssertionError('dispatched command')),
    patch.object(subprocess, 'check_output', side_effect=AssertionError('dispatched command')),
):
    try:
        getattr(module, {entry!r})(*([args] if {takes_args!r} else []))
    except RuntimeError as error:
        if str(error) == 'benchmark validation assertions must be enabled':
            raise SystemExit(0)
        raise
raise SystemExit('optimized execution was accepted')
"""
                result = subprocess.run([sys.executable, "-O", "-c", code], capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertFalse((self.root / "no-effects").exists())


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

    def test_cancelled_wait_kills_and_reaps_owned_group_without_losing_primary(self):
        from unittest.mock import Mock, call, patch
        import signal
        for index, primary in enumerate([KeyboardInterrupt("stop"), SystemExit(7), OSError("wait failed")]):
            with self.subTest(primary=primary):
                process = Mock(pid=12345)
                process.wait.side_effect = [primary, -signal.SIGKILL]
                timer = Mock(ident=1)
                with patch.object(self.paired.subprocess, 'Popen', return_value=process), \
                     patch.object(self.paired.threading, 'Timer', return_value=timer), \
                     patch.object(self.paired.os, 'killpg') as kill:
                    with self.assertRaises(type(primary)) as caught:
                        self.paired.run_command(['unused'], self.root / f'cancel-{index}', 5)
                self.assertIs(caught.exception, primary)
                kill.assert_called_once_with(process.pid, signal.SIGKILL)
                self.assertEqual(process.wait.call_args_list, [call(), call(timeout=5)])
                timer.cancel.assert_called_once_with()
                timer.join.assert_called_once_with()

    def test_cleanup_failures_are_chained_under_wait_cancellation(self):
        from unittest.mock import Mock, call, patch
        for boundary in ['kill', 'reap']:
            with self.subTest(boundary=boundary):
                primary = KeyboardInterrupt("primary")
                cleanup = RuntimeError("cleanup")
                process = Mock(pid=12345)
                process.wait.side_effect = [primary, cleanup if boundary == 'reap' else 0]
                timer = Mock(ident=1)
                with patch.object(self.paired.subprocess, 'Popen', return_value=process), \
                     patch.object(self.paired.threading, 'Timer', return_value=timer), \
                     patch.object(self.paired.os, 'killpg', side_effect=cleanup if boundary == 'kill' else None):
                    with self.assertRaises(KeyboardInterrupt) as caught:
                        self.paired.run_command(['unused'], self.root / f'cleanup-{boundary}', 5)
                self.assertIs(caught.exception, primary)
                self.assertIs(caught.exception.__cause__, cleanup)
                self.assertEqual(process.wait.call_args_list, [call(), call(timeout=5)])
                timer.cancel.assert_called_once_with()
                timer.join.assert_called_once_with()

    def test_timer_start_failure_also_reaps_the_created_process(self):
        from unittest.mock import Mock, call, patch
        for started in [False, True]:
            primary = RuntimeError("timer start failed")
            process = Mock(pid=12345)
            process.wait.return_value = 0
            timer = Mock(ident=1 if started else None)
            timer.start.side_effect = primary
            with patch.object(self.paired.subprocess, 'Popen', return_value=process), \
                 patch.object(self.paired.threading, 'Timer', return_value=timer), \
                 patch.object(self.paired.os, 'killpg') as kill:
                with self.assertRaises(RuntimeError) as caught:
                    self.paired.run_command(['unused'], self.root / f'timer-{started}', 5)
            self.assertIs(caught.exception, primary)
            kill.assert_called_once()
            self.assertEqual(process.wait.call_args_list, [call(timeout=5)])
            timer.cancel.assert_called_once_with()
            self.assertEqual(timer.join.call_count, int(started))

    def test_timer_creation_failure_reaps_the_created_process(self):
        from unittest.mock import Mock, call, patch
        primary = MemoryError("timer allocation")
        process = Mock(pid=12345)
        process.wait.return_value = 0
        with patch.object(self.paired.subprocess, 'Popen', return_value=process), \
             patch.object(self.paired.threading, 'Timer', side_effect=primary), \
             patch.object(self.paired.os, 'killpg') as kill:
            with self.assertRaises(MemoryError) as caught:
                self.paired.run_command(['unused'], self.root / 'timer-allocation', 5)
        self.assertIs(caught.exception, primary)
        kill.assert_called_once()
        self.assertEqual(process.wait.call_args_list, [call(timeout=5)])

    def test_timer_cleanup_failure_does_not_replace_wait_cancellation(self):
        from unittest.mock import Mock, patch
        primary = KeyboardInterrupt("primary")
        cleanup = RuntimeError("timer join")
        process = Mock(pid=12345)
        process.wait.side_effect = [primary, 0]
        timer = Mock(ident=1)
        timer.join.side_effect = cleanup
        with patch.object(self.paired.subprocess, 'Popen', return_value=process), \
             patch.object(self.paired.threading, 'Timer', return_value=timer), \
             patch.object(self.paired.os, 'killpg'):
            with self.assertRaises(KeyboardInterrupt) as caught:
                self.paired.run_command(['unused'], self.root / 'timer-cleanup', 5)
        self.assertIs(caught.exception, primary)
        self.assertIs(caught.exception.__cause__, cleanup)

    def test_real_child_is_reaped_before_cancellation_is_returned(self):
        from unittest.mock import patch
        import signal
        original_wait = self.paired.subprocess.Popen.wait
        processes = []
        primary = KeyboardInterrupt("cancel live child")
        def interrupted_wait(process, timeout=None):
            if timeout is None and not processes:
                processes.append(process)
                raise primary
            return original_wait(process, timeout=timeout)
        try:
            with patch.object(self.paired.subprocess.Popen, 'wait', interrupted_wait):
                with self.assertRaises(KeyboardInterrupt) as caught:
                    self.paired.run_command(['sleep', '30'], self.root / 'real-cancel', 5)
            self.assertIs(caught.exception, primary)
            self.assertEqual(len(processes), 1)
            self.assertEqual(processes[0].returncode, -signal.SIGKILL)
        finally:
            for process in processes:
                if process.returncode is None:
                    self.paired.os.killpg(process.pid, signal.SIGKILL)
                    original_wait(process, timeout=5)

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

    def test_relative_port_target_directory_survives_disposable_checkout(self):
        import runpy
        from unittest.mock import patch
        main = runpy.run_path(str(self.repo / 'ci/ports/check-rust.py'))['main']
        previous = Path.cwd()
        os.chdir(self.root)
        self.addCleanup(os.chdir, previous)

        commands = []

        def run(command, *args, **kwargs):
            # The gate's local wrapper passes commands as positional arguments;
            # its final parity checks pass a list directly.
            command = [command, *args] if isinstance(command, str) else list(command)
            commands.append(command)
            cwd = Path(kwargs.get('cwd', self.root))
            if command[:2] == ['git', 'checkout']:
                (cwd / 'Cargo.toml').write_text('vnfs = { version = "0.0.29", features = ["posix"] }\n')
            if command[:2] == ['cargo', 'metadata']:
                return subprocess.CompletedProcess(command, 0, stdout=json.dumps({
                    'packages': [{'name': 'vnfs', 'manifest_path': str(self.root / 'crates/vnfs/Cargo.toml')}]
                }).encode())
            if command[:2] == ['cargo', 'build']:
                binary = cwd / kwargs['env']['CARGO_TARGET_DIR'] / 'debug/find'
                binary.parent.mkdir(parents=True, exist_ok=True)
                binary.write_text('fake binary')
            if len(command) > 1 and str(command[0]).endswith('/debug/find'):
                if not Path(command[0]).is_file():
                    raise FileNotFoundError(command[0])
            return subprocess.CompletedProcess(command, 0, stdout=b'parity\n')

        migration = self.root / 'coordinated.patch'
        for configured in [False, True]:
            commands.clear()
            with patch.dict(os.environ, CARGO_TARGET_DIR='cache',
                            VFSI_PORT_MIGRATION_PATCH=str(migration) if configured else ''), patch.object(
                sys, 'argv', ['check-rust.py', 'unused', 'a' * 40, 'findutils']
            ), patch.object(subprocess, 'run', side_effect=run):
                main()
            self.assertTrue((self.root / 'cache/debug/find').is_file())
            applied = [command for command in commands if command[:2] == ['git', 'apply']]
            # Published adoption is complete: even a stale transition variable
            # must not modify the committed source selected by the registry.
            self.assertEqual(applied, [])


class DevelopmentPortRunnerTests(unittest.TestCase):
    def test_sdk_target_is_shared_but_git_build_keeps_its_archive_path(self):
        source = Path(__file__).resolve().parent / "test-ports-local.sh"
        with tempfile.TemporaryDirectory(prefix="vfsi-dev-ports-") as directory:
            temp = Path(directory)
            platform = temp / "platform"
            (platform / "scripts").mkdir(parents=True)
            runner = platform / "scripts" / source.name
            shutil.copyfile(source, runner)
            ports = temp / "ports"
            for port in ("coreutils", "findutils", "git", "rsync"):
                (ports / f"vfsi-port-{port}" / ".git").mkdir(parents=True)
            rsync = ports / "vfsi-port-rsync"
            (rsync / "config.h").write_text("#define SUPPORT_VFSI 1\n")
            for script in (
                ports / "vfsi-port-git/contrib/vfsi/test-port.sh",
                rsync / "support/test-vfsi-port.sh",
                platform / "scripts/test-port-listing-overflow.sh",
            ):
                script.parent.mkdir(parents=True, exist_ok=True)
                script.write_text('printf "sdk %s\\n" "$VFSI_LIBRARY" >>"$CALL_LOG"\n')
            bins = temp / "bin"
            bins.mkdir()
            cargo = bins / "cargo"
            cargo.write_text(
                '#!/bin/sh\nprintf "cargo %s\\n" "$CARGO_TARGET_DIR" >>"$CALL_LOG"\n'
                'if [ "$1" = metadata ]; then printf "%s\\n" "$TEST_METADATA"; fi\n'
            )
            make = bins / "make"
            make.write_text('#!/bin/sh\nprintf "make %s\\n" "${CARGO_TARGET_DIR:-unset}" >>"$CALL_LOG"\n')
            for stub in (cargo, make):
                stub.chmod(0o755)
            log = temp / "calls"
            result = subprocess.run(
                ["bash", str(runner), str(ports)], cwd=temp,
                env=dict(os.environ, PATH=str(bins) + os.pathsep + os.environ["PATH"],
                         CARGO_TARGET_DIR="cache", CALL_LOG=str(log),
                         TEST_METADATA=json.dumps({"packages": [{
                             "name": "vnfs", "manifest_path": str(platform / "crates/vnfs/Cargo.toml"),
                         }]})),
                capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            calls = log.read_text().splitlines()
            target = temp / "cache"
            targets = [call for call in calls if call.startswith("cargo ")]
            self.assertTrue(targets)
            self.assertEqual(set(targets), {f"cargo {target}"})
            self.assertEqual([call for call in calls if call.startswith("make ")],
                             ["make unset", f"make {target}"])
            self.assertEqual([call for call in calls if call.startswith("sdk ")],
                             [f"sdk {target}/debug/libvfsi_c.so"] * 3)


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
