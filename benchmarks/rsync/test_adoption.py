"""Pure fixture and manifest checks; real transfers are a separate required gate."""

import argparse
import json
import os
import importlib.util
from pathlib import Path
import tempfile
import subprocess
import sys
import gzip
import audit_adoption
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("rsync_adoption", HERE / "adoption.py")
adoption = importlib.util.module_from_spec(spec)
spec.loader.exec_module(adoption)


class AdoptionTests(unittest.TestCase):
    def test_prepare_relative_paths_find_downloaded_headers(self):
        class PreparedHeaders(Exception):
            pass

        with tempfile.TemporaryDirectory() as temp:
            original = Path.cwd()
            os.chdir(temp)
            try:

                def snapshot(checkout, revision, output, name):
                    source = (output / (name + "-source")).resolve()
                    source.mkdir()
                    return source, {}

                def headers(command, *, cwd, env, **unused):
                    release = Path(env["VFSI_BUILD_DIR"])
                    if not release.is_absolute():
                        release = cwd / release
                    archive = (
                        release
                        / ("vfsi-c-" + adoption.SDK)
                        / ("vfsi-c-" + adoption.SDK + ".crate")
                    )
                    archive.parent.mkdir(parents=True)
                    archive.write_bytes(b"headers")

                def verify(path):
                    self.assertEqual(path.read_bytes(), b"headers")
                    raise PreparedHeaders

                with patch.object(
                    adoption.development, "snapshot", side_effect=snapshot
                ), patch.object(
                    adoption.subprocess, "run", side_effect=headers
                ), patch.object(
                    adoption.scan, "sha", side_effect=verify
                ):
                    with self.assertRaises(PreparedHeaders):
                        adoption.prepare(
                            argparse.Namespace(
                                output=Path("build"),
                                target_dir=Path("cache"),
                                checkout=Path("checkout"),
                            )
                        )
            finally:
                os.chdir(original)

    def test_capture_startup_interrupt_reaps_the_child(self):
        real_popen = subprocess.Popen
        children = []

        def launch(*args, **kwargs):
            child = real_popen(
                [sys.executable, "-c", "import time; time.sleep(30)"], **kwargs
            )
            children.append(child)
            return child

        with tempfile.TemporaryDirectory() as temp:
            try:
                with patch.object(
                    adoption.subprocess, "Popen", side_effect=launch
                ), patch.object(
                    adoption.time, "sleep", side_effect=SystemExit("interrupted")
                ):
                    with self.assertRaises(SystemExit):
                        adoption.start_capture("unused", Path(temp) / "trial")
                self.assertIsNotNone(
                    children[0].poll(), "capture child survived startup interruption"
                )
            finally:
                for child in children:
                    if child.poll() is None:
                        child.kill()
                    child.wait()

    def test_audit_requires_capture_for_completed_transfer(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "results.json").write_text(
                json.dumps(
                    {
                        "verified": True,
                        "transfers": [{"scenario": "create", "wire": None}],
                    }
                )
            )
            with self.assertRaisesRegex(AssertionError, "missing capture"):
                audit_adoption.audit(root, root, root / "proof.json")
            self.assertFalse((root / "proof.json").exists())

    def test_independent_recipe_counts_match_the_frozen_workload(self):
        import hashlib

        payload = (b"VFSI du metadata benchmark\n" * 8)[:128]
        cases = audit_adoption.expected_cases(
            dict(
                branches=6,
                depth=4,
                files_per_directory=8,
                file_bytes=128,
                payload_sha256=hashlib.sha256(payload).hexdigest(),
            )
        )
        self.assertEqual(
            {name: len(rows) for name, rows in cases.items()},
            {
                "create": 13995,
                "update": 13996,
                "delete": 13986,
                "excludes": 13987,
                "symlinks": 13992,
                "partial-failure": 13993,
                "resumed": 13994,
                "fallback": 5,
            },
        )
        self.assertNotIn("blocked.bin", cases["partial-failure"])
        self.assertEqual(
            cases["symlinks"]["dangling"], ["symlink", 0o777, "missing-target"]
        )

    def test_audit_rejects_corrupt_capture_before_decoding(self):
        with tempfile.TemporaryDirectory() as temp:
            stem = Path(temp) / "trial"
            Path(str(stem) + ".capture.log").write_text("0 packets dropped by kernel\n")
            with gzip.open(str(stem) + ".pcap.gz", "wb") as stream:
                stream.write(b"not a valid capture")
            with patch.object(adoption.scan, "capture_summary") as decoder:
                with self.assertRaises(AssertionError):
                    audit_adoption.recheck_wire(stem, {"pcap_sha256": "bad"})
                decoder.assert_not_called()

    def test_audit_does_not_overwrite_prior_evidence(self):
        with tempfile.TemporaryDirectory() as temp:
            proof = Path(temp) / "proof.json"
            proof.write_text("preserve me")
            with self.assertRaises(FileExistsError):
                audit_adoption.audit(Path(temp), Path(temp), proof)
            self.assertEqual(proof.read_text(), "preserve me")

    def test_optimized_audit_cannot_report_unchecked_success(self):
        result = subprocess.run(
            [sys.executable, "-O", str(HERE / "audit_adoption.py"), "--help"],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("audit assertions must be enabled", result.stderr)

    def test_frozen_port_trace_requires_collect_boundary_and_retained_cursors(self):
        root = "vfsi: listdirs completed mode=cohort dirs=1 cursors=0 entries=2 phase=collect cohort_application_entries=0"
        child = "vfsi: listdirs completed mode=cohort dirs=1 cursors=1 entries=3 phase=collect cohort_application_entries=0"
        self.assertEqual(adoption.checked_trace(root + "\n" + child, "vfsi"), 5)
        self.assertEqual(adoption.checked_trace(root, "vfsi", partial=True), 2)
        legacy = root.replace("mode=cohort", "mode=paths")
        self.assertEqual(adoption.checked_trace(legacy, "old-library"), 2)
        for text in (
            root,
            child.replace("cursors=1", "cursors=2"),
            child.replace(
                "cohort_application_entries=0", "cohort_application_entries=1"
            ),
            child.split(" phase=")[0],
            child + "\nvfsi: listing fallback phase=collect",
        ):
            with self.subTest(text=text):
                with self.assertRaises(ValueError):
                    adoption.checked_trace(text, "vfsi")
        self.assertEqual(adoption.checked_trace("", "stock"), 0)
        diagnostic = "vfsi: cannot load /nonexistent-vfsi-adoption.so: cannot open shared object file"
        self.assertEqual(adoption.checked_trace(diagnostic, "missing-library"), 0)
        with self.assertRaises(ValueError):
            adoption.checked_trace("", "missing-library")
        with self.assertRaises(ValueError):
            adoption.checked_trace(diagnostic + "\n" + child, "missing-library")

    def test_mount_prepares_cwd_before_capture_and_restores_before_unmount(self):
        original = Path.cwd()
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "owned-mountpoint"
            root.mkdir()
            attached = [False]

            def checked(command, **unused):
                if command[0] == "mount":
                    attached[0] = True
                else:
                    self.assertEqual(Path.cwd(), original)
                    attached[0] = False
                return ""

            with patch.object(
                adoption.tempfile, "mkdtemp", return_value=str(root)
            ), patch.object(
                adoption, "mount_present", side_effect=lambda path: attached[0]
            ), patch.object(
                adoption.scan, "checked", side_effect=checked
            ):
                with self.assertRaisesRegex(ValueError, "body failed"):
                    with adoption.private_mount("/fixture") as mount:
                        self.assertEqual(Path.cwd(), mount)
                        raise ValueError("body failed")
            self.assertEqual(Path.cwd(), original)
            self.assertFalse(root.exists())

    def test_failed_unmount_never_recurses_into_an_attached_source(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "owned-mountpoint"
            root.mkdir()

            def checked(command, **unused):
                if command[0] == "umount":
                    raise RuntimeError("cannot detach")
                return ""

            with patch.object(
                adoption.tempfile, "mkdtemp", return_value=str(root)
            ), patch.object(adoption, "mount_present", return_value=True), patch.object(
                adoption.scan, "checked", side_effect=checked
            ), patch.object(
                adoption.shutil, "rmtree"
            ) as remove:
                with self.assertRaises(RuntimeError):
                    with adoption.private_mount("/fixture") as mount:
                        (mount / "source-data").write_bytes(b"preserve me")
                remove.assert_not_called()
            self.assertEqual((root / "mount/source-data").read_bytes(), b"preserve me")

    def test_manifest_covers_bytes_modes_and_symlinks_without_following(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            adoption.put(root / "file", b"abc", mode=0o640)
            (root / "link").symlink_to("file")
            (root / "dangling").symlink_to("absent")
            (root / "outside").symlink_to("/dev/null")
            rows = adoption.tree_manifest(root)
            self.assertEqual(rows["file"][:3], ["file", 0o640, 3])
            self.assertEqual(rows["link"], ["symlink", 0o777, "file"])
            self.assertEqual(rows["outside"], ["symlink", 0o777, "/dev/null"])
            before = rows["file"][3]
            (root / "file").write_bytes(b"xyz")
            self.assertNotEqual(adoption.tree_manifest(root)["file"][3], before)

    def test_expected_exclusions_preserve_other_paths(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for name in (
                "file",
                "visible.txt",
                "hidden.skip",
                "skip/child",
                "blocked.bin",
            ):
                adoption.put(root / name, b"data")
            rows = adoption.expected_manifest(root, excluded=True, blocked=True)
            self.assertEqual(set(rows), {".", "file", "visible.txt"})

    def test_progress_counts_regular_bytes_without_following_links(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            adoption.put(root / ".rsync-temporary", b"partial")
            (root / "outside").symlink_to("/dev/zero")
            self.assertEqual(adoption.partial_bytes(root), 7)
            (root / ".rsync-temporary").unlink()
            self.assertEqual(adoption.partial_bytes(root), 0)

    def test_manifest_digest_is_independent_of_enumeration_and_output_path(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            first = adoption.save_manifest(
                root / "one.gz", {"b": ["file", 1], "a": ["directory", 2]}
            )
            second = adoption.save_manifest(
                root / "two.gz", {"a": ["directory", 2], "b": ["file", 1]}
            )
            self.assertEqual(first, second)

    def test_deadline_validation_happens_before_creating_output(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "must-not-exist"
            for timeout, worker_timeout in [(0, 900), (601, 900), (1, 59), (1, 1801)]:
                with self.subTest(timeout=timeout, worker_timeout=worker_timeout):
                    with self.assertRaises(ValueError):
                        adoption.run(
                            argparse.Namespace(
                                timeout=timeout,
                                worker_timeout=worker_timeout,
                                output=output,
                            )
                        )
                    self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
