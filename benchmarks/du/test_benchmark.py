import importlib.util
import gzip
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock
import shutil

from generate import generate, geometry, fingerprint

spec = importlib.util.spec_from_file_location('runner', Path(__file__).with_name('run.py'))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


def row(src, sport, dport, kind, xid, programs='', procs='', ops=''):
    return '\t'.join(['1', src, sport, dport, kind, xid, programs, procs, ops, '']) + '\n'


class BenchmarkTests(unittest.TestCase):
    def test_geometry_and_mutation_detection(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / 'fixture'
            manifest = generate(root, 2, 2, 3, 128)
            self.assertEqual((manifest['directories'], manifest['regular_files']), (7, 21))
            self.assertEqual(manifest['logical_file_bytes'], 21 * 128)
            self.assertEqual(manifest['identity_sha256'], fingerprint(root / 'tree')['identity_sha256'])
            (root / 'tree/f000-metadata-small-file').write_bytes(b'changed')
            self.assertNotEqual(manifest['identity_sha256'], fingerprint(root / 'tree')['identity_sha256'])
            with self.assertRaises(FileExistsError):
                generate(root, 2, 2, 3, 128)

    def test_generator_bounds_before_creation(self):
        for arguments in [(0, 1, 1, 1), (16, 8, 128, 128), (1, 1, 1, -1)]:
            with self.assertRaises(ValueError):
                geometry(*arguments)

    def test_payload_boundaries(self):
        with tempfile.TemporaryDirectory() as temp:
            for size in [0, 1, 27, 128, 65536]:
                root = Path(temp) / str(size)
                manifest = generate(root, 1, 0, 1, size)
                self.assertEqual(manifest['logical_file_bytes'], size)

    def test_count_rpc_pdus_not_tcp_packets(self):
        lines = [row(runner.CLIENT, '4000', '2049', '0,0', '0x1,0x2', '100003,100003', '1,1', '53,22,26,53,22,9'),
                 row(runner.SERVER, '2049', '4000', '1,1', '0x1,0x2')]
        summary, records = runner.rpc_summary(lines)
        self.assertEqual(summary['rpc_calls'], 2)
        self.assertEqual(summary['procedures'], {'1': 2})
        self.assertEqual(summary['request_opcodes']['53'], 2)
        self.assertEqual(len(records), 2)

    def test_unmatched_or_ambiguous_capture_rejected(self):
        for lines in [[], [row(runner.CLIENT, '4', '2049', '0', '1', '100003', '1')],
                      [row(runner.CLIENT, '4', '2049', '0,1', '1,2', '100003', '1')],
                      [row(runner.CLIENT, '4', '2049', '0,0', '1,2', '100003', '1,1')]]:
            with self.assertRaises(ValueError):
                runner.rpc_summary(lines)

    def test_retries_count_as_actual_calls(self):
        call = row(runner.CLIENT, '4', '2049', '0', '1', '100003', '1')
        reply = row(runner.SERVER, '2049', '4', '1', '1')
        summary, _ = runner.rpc_summary([call, reply, call, reply])
        self.assertEqual(summary['rpc_calls'], 2)
        self.assertEqual(summary['unique_rpc_calls'], 1)
        self.assertEqual(summary['repeated_xids'], 1)

    def test_lock_and_release_frozen_together(self):
        baseline = json.loads(Path(__file__).with_name('baseline.json').read_text())
        self.assertEqual(runner.sha(Path(__file__).with_name('Cargo.lock')), baseline['cargo_lock_sha256'])
        self.assertEqual(baseline['packages']['vnfs'], '0.0.27')

    @unittest.skipUnless(shutil.which('tshark'), 'offline dissector integration requires tshark')
    def test_reserved_client_port_decoded_as_rpc(self):
        with tempfile.TemporaryDirectory() as temp:
            capture = Path(temp) / 'case.pcap'
            fixture = Path(__file__).parent / 'fixtures/rpc-source-port-705.pcap.gz'
            capture.write_bytes(gzip.decompress(fixture.read_bytes()))
            summary = runner.capture_summary(capture, Path(temp) / 'case')
            self.assertEqual(summary['rpc_calls'], 1)
            self.assertEqual(summary['procedures'], {'1': 1})
            rows = gzip.decompress((Path(temp) / 'case.rpc-fields.tsv.gz').read_bytes()).decode().splitlines()
            self.assertEqual(rows[1].split('\t')[2], '705')

    def test_cleanup_only_deletes_resources_it_created(self):
        for fail_on in [('ip', 'link', 'add'), ('ip', 'link', 'set')]:
            link, calls = runner.Link(), []
            def command(*args):
                calls.append(args)
                if args[:3] == fail_on:
                    raise RuntimeError('injected setup failure')
                return '[]' if args[:2] == ('ip', '-j') else ''
            with mock.patch.object(link, 'privileged', side_effect=command):
                with self.assertRaisesRegex(RuntimeError, 'injected'):
                    link.__enter__()
            self.assertIn(('ip', 'netns', 'del', link.ns), calls)
            self.assertEqual(('ip', 'link', 'del', link.host) in calls,
                             fail_on == ('ip', 'link', 'set'))


if __name__ == '__main__':
    unittest.main()
