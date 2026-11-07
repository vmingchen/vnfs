import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("rsync_development", Path(__file__).with_name("development.py"))
development = importlib.util.module_from_spec(spec)
spec.loader.exec_module(development)


class DevelopmentScanTests(unittest.TestCase):
    def test_cohort_trace_requires_positive_acceleration_and_rejects_partial_fallback(self):
        trace = "vfsi: listdirs completed mode=cohort dirs=24 cursors=23 entries=81\n"
        self.assertEqual(development.checked_trace(trace, "vfsi"), 81)
        self.assertEqual(development.checked_trace(trace + trace, "vfsi"), 162)
        for bad in [
            "", trace.replace("cohort", "paths"), trace.replace("entries=81", "entries=0"),
            trace.replace("cursors=23", "cursors=25"), trace.replace("dirs=24", "dirs=33"),
            trace + "vfsi: directory cohort failed: 5; using POSIX scan\n",
        ]:
            with self.subTest(trace=bad), self.assertRaises(ValueError):
                development.checked_trace(bad, "vfsi")
        for method in ("kernel", "unsupported", "missing-library"):
            with self.subTest(method=method):
                with self.assertRaises(ValueError):
                    development.checked_trace(trace, method)
                self.assertEqual(development.checked_trace("vfsi: using POSIX scan\n", method), 0)
        # Supporting the new format must not relax the published runner's gate.
        with self.assertRaises(ValueError):
            development.scan.checked_trace(trace, "vfsi")
        self.assertEqual(development.scan.checked_trace("vfsi: listdirs completed entries=81\n", "vfsi"), 81)

    def test_cohort_trace_requires_aggregate_cursor_reuse_after_root_scan(self):
        root = "vfsi: listdirs completed mode=cohort dirs=1 cursors=0 entries=5\n"
        children = "vfsi: listdirs completed mode=cohort dirs=24 cursors=23 entries=81\n"
        self.assertEqual(development.checked_trace(root + children, "vfsi"), 86)
        self.assertEqual(development.checked_trace(children + root, "vfsi"), 86)
        for trace in (root, root + root):
            with self.subTest(trace=trace), self.assertRaisesRegex(ValueError, "retained cursor"):
                development.checked_trace(trace, "vfsi")

    def fixture(self, root):
        repo = root / "repo"
        repo.mkdir()
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        (repo / "input").write_text("committed\n")
        subprocess.run(["git", "-C", str(repo), "add", "input"], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "user.name=SDK fixture",
                        "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false",
                        "commit", "-qm", "fixture"], check=True)
        revision = subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip()
        return repo, revision

    def test_exact_revision_snapshot_ignores_current_working_tree_edits(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            repo, revision = self.fixture(root)
            (repo / "input").write_text("local edit\n")
            output = root / "output"
            output.mkdir()
            source, evidence = development.snapshot(repo, revision, output, "sdk")
            self.assertEqual((source / "input").read_text(), "committed\n")
            self.assertEqual((repo / "input").read_text(), "local edit\n")
            self.assertEqual(evidence["revision"], revision)
            self.assertEqual(evidence["archive_sha256"], development.scan.sha(output / "sdk.tar"))
            self.assertNotIn("patch_sha256", evidence)

    def test_explicit_patch_is_applied_only_to_snapshot_and_recorded(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            repo, revision = self.fixture(root)
            (repo / "input").write_text("candidate\n")
            patch = root / "candidate.patch"
            patch.write_bytes(subprocess.check_output(["git", "-C", str(repo), "diff", "--binary"]))
            output = root / "output"
            output.mkdir()
            source, evidence = development.snapshot(repo, revision, output, "sdk", patch)
            self.assertEqual((source / "input").read_text(), "candidate\n")
            self.assertEqual((repo / "input").read_text(), "candidate\n")
            self.assertEqual(evidence["patch_sha256"], development.scan.sha(patch))
            self.assertEqual(subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip(),
                             revision)

    def test_patch_in_nested_worktree_output_changes_only_the_snapshot(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            repo, revision = self.fixture(root)
            original = repo / "input"
            original.write_text("candidate\n")
            patch = root / "candidate.patch"
            patch.write_bytes(subprocess.check_output(["git", "-C", str(repo), "diff", "--binary"]))
            original.write_text("unrelated local edit\n")
            before = subprocess.check_output(["git", "-C", str(repo), "diff", "HEAD", "--", "input"])
            output = repo / "benchmark-output"
            output.mkdir()
            source, evidence = development.snapshot(repo, revision, output, "sdk", patch)
            self.assertEqual((source / "input").read_text(), "candidate\n")
            self.assertEqual(original.read_text(), "unrelated local edit\n")
            self.assertEqual(evidence["patch_sha256"], development.scan.sha(patch))
            self.assertEqual(subprocess.check_output(["git", "-C", str(repo), "diff", "HEAD", "--", "input"]), before)
            subprocess.run(["git", "-C", str(repo), "diff", "--cached", "--exit-code"], check=True)
            self.assertEqual(subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip(),
                             revision)

    def test_inapplicable_patch_in_nested_worktree_output_is_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            repo, revision = self.fixture(root)
            patch = root / "candidate.patch"
            patch.write_text(
                "diff --git a/input b/input\n--- a/input\n+++ b/input\n"
                "@@ -1 +1 @@\n-missing old content\n+candidate\n"
            )
            output = repo / "benchmark-output"
            output.mkdir()
            with self.assertRaises(subprocess.CalledProcessError):
                development.snapshot(repo, revision, output, "sdk", patch)
            self.assertEqual((repo / "input").read_text(), "committed\n")
            self.assertEqual((output / "sdk-source/input").read_text(), "committed\n")

    def test_invalid_or_moving_revision_cannot_create_snapshot(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp)
            for revision in ("HEAD", "main", "abc123", "../input"):
                with self.subTest(revision=revision), self.assertRaisesRegex(ValueError, "full commit hash"):
                    development.snapshot(output, revision, output, "sdk")
            self.assertEqual(list(output.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
