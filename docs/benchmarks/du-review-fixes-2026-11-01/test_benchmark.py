import importlib.util
import gzip
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock
import shutil
import subprocess
from types import SimpleNamespace

from generate import generate, geometry, fingerprint

spec = importlib.util.spec_from_file_location('runner', Path(__file__).with_name('run.py'))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


def row(src, sport, dport, kind, xid, programs='', procs='', ops=''):
    return '\t'.join(['1', src, sport, dport, kind, xid, programs, procs, ops, '']) + '\n'


class BenchmarkTests(unittest.TestCase):
    def test_comparison_reaches_every_trial_after_building_and_checks_parity(self):
        import compare
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            fixture = root/'fixture'
            generate(fixture, 1, 0, 1, 1)
            checkout = root/'candidate'
            checkout.mkdir()
            (checkout/'Cargo.lock').write_text('candidate lock')
            binary = checkout/'du'
            binary.write_bytes(b'candidate executable')
            published = root/'published.json'
            published.write_text(json.dumps({'provenance': {'binary_sha256': 'frozen'}}))
            args = SimpleNamespace(pairs=3, captures=1, output=root/'results', export='/fixture',
                frozen=root/'frozen', candidate=checkout, library=root/'library',
                fixture=fixture, baseline_results=published)
            payload = b'4096\ttree/f000-metadata-small-file\n8192\ttree\n'
            parity = hashlib.sha256(b'\n'.join(sorted(payload.splitlines()))+b'\n').hexdigest()
            commands = []
            def trial(*command):
                commands.append(command)
                stem = Path(command[command.index('--output')+1])
                with gzip.open(str(stem)+'.stdout.gz', 'wb') as stream:
                    stream.write(payload)
                row = dict(stdout_sorted_sha256=parity, stdout_lines=2, elapsed_ns=1000000)
                if '--capture' in command:
                    row['wire'] = {'rpc_calls': 1}
                Path(str(stem)+'.json').write_text(json.dumps(row))
            link = mock.Mock()
            link.delay.return_value = {'ping_rtt_median_ms': 1.0}
            link.client.side_effect = trial
            with mock.patch.object(compare.run, 'build', return_value=(root/'frozen-du',
                    {'binary_sha256': 'frozen'})), \
                 mock.patch.object(compare, 'build_candidate', return_value=(binary,
                    {'binary_sha256': runner.sha(binary)})), \
                 mock.patch.object(compare.run, 'checked', return_value='source'), \
                 mock.patch.object(compare.run, 'Link') as network, \
                 mock.patch('builtins.print'):
                network.return_value.__enter__.return_value = link
                compare.main(args)
            result = json.loads((args.output/'results.json').read_text())
            self.assertTrue(result['output_parity'])
            self.assertTrue(result['post_order'])
            self.assertEqual(len(commands), 30)
            self.assertEqual(len(result['samples']), 30)
            self.assertEqual({s['variant'] for s in result['samples']},
                {'kernel', 'frozen-vfsi', 'candidate-vfsi'})
            self.assertEqual(sum(command[command.index('--binary')+1] == str(binary) for command in commands), 10)

    def test_candidate_is_rebuilt_before_source_provenance_is_recorded(self):
        import compare
        class SnapshotReached(Exception):
            pass
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            checkout = root / 'candidate'
            stale = checkout / 'target/release/du'
            stale.parent.mkdir(parents=True)
            stale.write_bytes(b'stale executable')
            # Cargo can choose another target directory; use its artifact path.
            rebuilt = root / 'custom-target/release/du'
            published = root / 'published.json'
            published.write_text(json.dumps({'provenance': {'binary_sha256': 'frozen'}}))
            args = SimpleNamespace(pairs=3, captures=1, output=root/'results',
                frozen=root/'frozen', candidate=checkout, library=root/'library',
                baseline_results=published)
            def build(command, **kwargs):
                self.assertEqual(command[:2], ['cargo', 'build'])
                for required in ['--release', '--locked', '--features', 'vnfs']:
                    self.assertIn(required, command)
                self.assertEqual(kwargs['cwd'], checkout.resolve())
                self.assertTrue(kwargs['check'])
                rebuilt.parent.mkdir(parents=True)
                rebuilt.write_bytes(b'rebuilt executable')
                kwargs['stdout'].write(json.dumps({'reason': 'compiler-artifact',
                    'target': {'name': 'du', 'kind': ['bin']}, 'executable': str(rebuilt)})+'\n')
            def snapshot(command, **kwargs):
                if command[0] == 'git':
                    self.assertTrue(rebuilt.exists(), 'candidate must be rebuilt before snapshot')
                    raise SnapshotReached
                return 'toolchain'
            with mock.patch.object(compare.run, 'build', return_value=(root/'frozen-du',
                    {'binary_sha256': 'frozen'})), \
                 mock.patch('subprocess.run', side_effect=build) as build_command, \
                 mock.patch.object(compare.run, 'checked', side_effect=snapshot), \
                 mock.patch.object(compare.run, 'sha', wraps=compare.run.sha) as digest:
                with self.assertRaises(SnapshotReached):
                    compare.main(args)
                self.assertEqual(build_command.call_count, 1)
                self.assertIn(mock.call(rebuilt), digest.call_args_list)

    def test_candidate_build_failure_stops_before_source_snapshot_or_measurement(self):
        import compare
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            checkout = root/'candidate'
            (checkout/'target/release').mkdir(parents=True)
            (checkout/'target/release/du').write_bytes(b'stale executable')
            (checkout/'Cargo.lock').write_text('lock')
            published = root/'published.json'
            published.write_text(json.dumps({'provenance': {'binary_sha256': 'frozen'}}))
            args = SimpleNamespace(pairs=3, captures=1, output=root/'results',
                frozen=root/'frozen', candidate=checkout, library=root/'library',
                fixture=root/'fixture', baseline_results=published)
            with mock.patch.object(compare.run, 'build', return_value=(root/'frozen-du',
                    {'binary_sha256': 'frozen'})), \
                 mock.patch('subprocess.run', side_effect=subprocess.CalledProcessError(101, ['cargo','build'])), \
                 mock.patch.object(compare.run, 'checked', return_value='') as snapshot, \
                 mock.patch.object(compare.run, 'Link') as link:
                with self.assertRaises(subprocess.CalledProcessError):
                    compare.main(args)
                snapshot.assert_not_called()
                link.assert_not_called()

    def test_comparison_tables_require_three_timings_and_a_capture_per_variant(self):
        from report import table
        variants = ['kernel', 'frozen-vfsi', 'candidate-vfsi']
        def report():
            samples = []
            for variant in variants:
                for pair in range(3):
                    samples.append(dict(regime='native', variant=variant, phase='timing',
                        pair=pair, elapsed_ns=1000000))
                samples.append(dict(regime='native', variant=variant, phase='capture',
                    pair=0, wire={'rpc_calls': 1}))
            return dict(candidate={}, regimes={'native': {}}, samples=samples)
        self.assertIn('1.000', table(report()))
        for variant in variants:
            for count in [0, 1, 2]:
                with self.subTest(variant=variant, timings=count):
                    data = report()
                    data['samples'] = [s for s in data['samples'] if
                        s['variant'] != variant or s['phase'] != 'timing' or s['pair'] < count]
                    with self.assertRaisesRegex(ValueError, 'insufficient publishable samples'):
                        table(data)
            data = report()
            data['samples'] = [s for s in data['samples'] if
                s['variant'] != variant or s['phase'] != 'capture']
            with self.assertRaisesRegex(ValueError, 'insufficient publishable samples'):
                table(data)

    def test_candidate_build_without_du_artifact_never_uses_cached_executable(self):
        from compare import build_candidate
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            checkout = root/'candidate'
            (checkout/'target/release').mkdir(parents=True)
            (checkout/'target/release/du').write_bytes(b'stale executable')
            def build(command, **kwargs):
                kwargs['stdout'].write(json.dumps({'reason': 'build-finished', 'success': True})+'\n')
            with mock.patch('subprocess.run', side_effect=build):
                with self.assertRaisesRegex(RuntimeError, 'exactly one du executable'):
                    build_candidate(checkout, root)

    def test_candidate_postorder_guard_rejects_parent_before_descendant(self):
        from compare import postorder
        with tempfile.TemporaryDirectory() as temp:
            stem = Path(temp) / 'trial'
            for data, valid in [
                (b'1\ttree/sub/file\n2\ttree/sub\n3\ttree\n', True),
                (b'3\ttree\n1\ttree/sub/file\n2\ttree/sub\n', False),
                (b'2\ttree/sub\n1\ttree/sub/file\n3\ttree\n', False),
            ]:
                with gzip.open(str(stem) + '.stdout.gz', 'wb') as out:
                    out.write(data)
                if valid:
                    postorder(stem)
                else:
                    with self.assertRaisesRegex(ValueError, 'directory precedes'):
                        postorder(stem)

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
