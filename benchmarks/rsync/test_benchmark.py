import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('rsync_benchmark', Path(__file__).with_name('run.py'))
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)


class FrozenScanTests(unittest.TestCase):
    def test_parity_gate_rejects_changed_output_missing_entries_and_silent_fallback(self):
        row = dict(stdout_sha256='same', stdout_lines=10, traced_entries=20)
        self.assertEqual(benchmark.validate_sample(row, 'vfsi', 10, 'same'), 'same')
        for change in (dict(stdout_sha256='changed'), dict(stdout_lines=9), dict(traced_entries=0)):
            with self.subTest(change=change), self.assertRaises(ValueError):
                benchmark.validate_sample(dict(row, **change), 'vfsi', 10, 'same')
        for method in ('kernel', 'missing-library', 'unsupported'):
            with self.subTest(method=method), self.assertRaises(ValueError):
                benchmark.validate_sample(row, method, 10, 'same')
            benchmark.validate_sample(dict(row, traced_entries=0), method, 10, 'same')
        self.assertEqual(benchmark.trace_entries('warning\nvfsi: listdirs completed entries=2\nvfsi: listdirs completed entries=3\n'), 5)
        self.assertEqual(benchmark.trace_entries('vfsi: directory listing failed; using POSIX scan\n'), 0)
        with self.assertRaisesRegex(ValueError, 'partial POSIX fallback'):
            benchmark.checked_trace('vfsi: listdirs completed entries=2\nvfsi: directory listing failed; using POSIX scan\n', 'vfsi')
        for method in ('kernel', 'missing-library', 'unsupported'):
            benchmark.checked_trace('vfsi: using POSIX scan\n', method)
        self.assertEqual(benchmark.checked_trace('vfsi: listdirs completed entries=3\n', 'vfsi'), 3)


    def test_wrong_port_revision_or_source_edits_stop_before_any_build(self):
        baseline = json.loads((benchmark.HERE/'baseline.json').read_text())
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for responses in (['wrong\n'], [baseline['port_revision']+'\n', 'source diff']):
                with self.subTest(responses=responses), \
                     patch.object(benchmark, 'checked', side_effect=responses), \
                     patch.object(benchmark.subprocess, 'run') as build, \
                     self.assertRaises(ValueError):
                    benchmark.build(root, root)
                build.assert_not_called()


if __name__ == '__main__':
    unittest.main()
