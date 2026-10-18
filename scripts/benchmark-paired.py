#!/usr/bin/env python3
"""Compare explicit commands on one fixture; never build or reshape the network."""
import argparse
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import random
import shlex
import shutil
import signal
import statistics
import stat
import subprocess
import threading
import time


def digest(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def command(template, values):
    # Expand after tokenization: spaces in paths cannot inject arguments.
    return [part.format_map(values) for part in shlex.split(template)]


def run_command(argv, stem, timeout, cpu=None):
    if cpu is not None:
        argv = ['taskset', '-c', str(cpu), *argv]
    with stem.with_suffix('.stdout').open('xb') as out, stem.with_suffix('.stderr').open('xb') as err:
        start = time.perf_counter_ns()
        process = subprocess.Popen(argv, stdout=out, stderr=err, start_new_session=True)
        expired = threading.Event()
        def terminate():
            expired.set()
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        deadline = threading.Timer(timeout, terminate)
        deadline.start()
        try:
            # wait(timeout=...) polls on POSIX, quantizing short app timings.
            status = process.wait()
            elapsed = time.perf_counter_ns() - start
        finally:
            deadline.cancel()
            deadline.join()
        if expired.is_set():
            raise RuntimeError(f'command timed out; logs: {stem}')
    if status:
        raise RuntimeError(f'command exited {status}; logs: {stem}')
    return {'command': argv, 'elapsed_ns': elapsed,
            'stdout_sha256': digest(stem.with_suffix('.stdout'))}


def comparison(ratios):
    rng = random.Random(104729)
    medians = sorted(statistics.median(rng.choices(ratios, k=len(ratios))) for _ in range(2000))
    return {'paired_speedups': ratios, 'median_speedup': statistics.median(ratios),
            'bootstrap_95_percent_interval': [medians[49], medians[1949]]}


def fixture_objects(root):
    objects = []
    for parent, directories, files in os.walk(root, followlinks=False):
        for name in sorted([*directories, *files]):
            path = Path(parent) / name
            metadata = path.lstat()
            objects.append((str(path.relative_to(root)), metadata.st_dev, metadata.st_ino))
    return sorted(objects)


def evidence(path):
    """Inner operation timings, when the command is our structured I/O driver."""
    if not path.exists():
        return None
    value = json.loads(path.read_text())
    if not isinstance(value.get('cases'), list):
        raise RuntimeError(f'unsupported structured report: {path}')
    return value


def metrics(before, after):
    if (before is None) != (after is None):
        raise RuntimeError('only one command produced a structured operation report')
    if before is None:
        return {}
    if before['environment']['fixture_objects'] != after['environment']['fixture_objects']:
        raise RuntimeError('benchmark fixture objects differ between commands')
    keyed = lambda report: {(c['backend'], c['case']): c for c in report['cases']}
    a, b = keyed(before), keyed(after)
    if a.keys() != b.keys() or len(a) != len(before['cases']) or len(b) != len(after['cases']):
        raise RuntimeError('structured report case sets differ or contain duplicates')
    result = {}
    for key in a:
        for field in ('rounds', 'bytes_per_call', 'requests_per_call', 'request_bytes', 'files', 'cache', 'allocation'):
            if a[key][field] != b[key][field]:
                raise RuntimeError(f'case geometry differs: {key}/{field}')
        if min(a[key]['p50_ns'], b[key]['p50_ns']) <= 0:
            raise RuntimeError('invalid operation timing')
        result['/'.join(key)] = a[key]['p50_ns'] / b[key]['p50_ns']
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', required=True, help='quoted command; {fixture}/{report} placeholders supported')
    parser.add_argument('--candidate', required=True)
    parser.add_argument('--fixture', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True, help='new result directory')
    parser.add_argument('--prepare', help='run once, outside measurement, before all pairs')
    parser.add_argument('--before-each', help='explicit cache/namespace setup outside each measurement')
    parser.add_argument('--cache-state', choices=['warm', 'advisory-evicted', 'mixed', 'application-defined'], required=True)
    parser.add_argument('--rtt-ms', type=float, help='externally established RTT label, not measured or configured here')
    parser.add_argument('--pairs', type=int, default=9)
    parser.add_argument('--timeout', type=float, default=180)
    parser.add_argument('--cpu', type=int)
    parser.add_argument('--check-output', action='store_true', help='require identical stdout for every application invocation')
    parser.add_argument('--max-regression-percent', type=float, help='fail only if the speedup interval is entirely beyond this slowdown')
    args = parser.parse_args(argv)
    if (args.pairs < 3 or args.pairs > 1000 or not math.isfinite(args.timeout) or args.timeout <= 0
            or args.rtt_ms is not None and not 0 <= args.rtt_ms < float('inf')
            or args.max_regression_percent is not None and not 0 < args.max_regression_percent < float('inf')):
        parser.error('invalid pairs, timeout, RTT, or regression threshold')
    if args.cpu is not None and args.cpu not in os.sched_getaffinity(0):
        parser.error('CPU is outside process affinity')
    args.output = args.output.resolve()
    args.fixture = args.fixture.resolve()
    if args.output == args.fixture or args.fixture in args.output.parents:
        parser.error('keep result files outside the measured fixture')
    # Lock the entire pairing sequence, not just one command at a time.
    descriptor = os.open(args.fixture.parent / ('.' + args.fixture.name + '.paired.lock'),
                         os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    with os.fdopen(descriptor, 'w') as lock:
        if not stat.S_ISREG(os.fstat(lock.fileno()).st_mode):
            raise RuntimeError('comparison lock must be a regular file')
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        perform(args)


def perform(args):
    args.output.mkdir(parents=True, exist_ok=False)
    values = {'fixture': str(args.fixture), 'report': str(args.output / 'prepare.json')}
    templates = {'baseline': args.baseline, 'candidate': args.candidate}
    binaries = {}
    for label, template in templates.items():
        tokens = command(template, values)
        if not tokens:
            raise RuntimeError('empty benchmark command')
        resolved = shutil.which(tokens[0])
        if not resolved:
            raise RuntimeError(f'command executable not found: {tokens[0]}')
        files = {str(Path(resolved).resolve())}
        for token in tokens[1:]:
            try:
                if Path(token).is_file():
                    files.add(str(Path(token).resolve()))
            except OSError:
                pass  # A long argument need not be a filesystem path.
        binaries[label] = {path: digest(path) for path in sorted(files)}
    if args.prepare:
        run_command(command(args.prepare, values), args.output / 'prepare', args.timeout, args.cpu)
    if not args.fixture.is_dir():
        raise RuntimeError('prepare must create the fixture, or supply an existing fixture')
    objects = fixture_objects(args.fixture)
    records, comparisons, output_hash = [], {}, None
    for pair in range(args.pairs):
        samples = {}
        # AB / BA / AB / BA => ABBA across adjacent pairs.
        order = ('baseline', 'candidate') if pair % 2 == 0 else ('candidate', 'baseline')
        for label in order:
            stem = args.output / f'{pair:03}-{label}'
            values['report'] = str(stem.with_suffix('.json'))
            if args.before_each:
                run_command(command(args.before_each, values), args.output / f'{pair:03}-{label}-setup', args.timeout, args.cpu)
            if fixture_objects(args.fixture) != objects:
                raise RuntimeError('fixture objects changed during setup')
            sample = run_command(command(templates[label], values), stem, args.timeout, args.cpu)
            if fixture_objects(args.fixture) != objects:
                raise RuntimeError('fixture objects changed during measurement')
            if any(digest(path) != checksum for path, checksum in binaries[label].items()):
                raise RuntimeError('command binary/input changed during comparison')
            if args.check_output:
                output_hash = output_hash or sample['stdout_sha256']
                if sample['stdout_sha256'] != output_hash:
                    raise RuntimeError(f'application outputs differ; logs: {stem}')
            sample['report'] = evidence(stem.with_suffix('.json'))
            samples[label] = sample
            records.append({'pair': pair, 'label': label, **sample})
        ratios = metrics(samples['baseline']['report'], samples['candidate']['report'])
        ratios['command_end_to_end'] = samples['baseline']['elapsed_ns'] / samples['candidate']['elapsed_ns']
        for name, ratio in ratios.items():
            comparisons.setdefault(name, []).append(ratio)
    summaries = {name: comparison(ratios) for name, ratios in comparisons.items()}
    result = {'schema': 1, 'binaries': binaries, 'cache_state': args.cache_state,
              'externally_established_rtt_ms': args.rtt_ms, 'cpu': args.cpu,
              'fixture': str(args.fixture), 'fixture_objects': objects,
              'samples': records, 'comparisons': summaries,
              'notes': ['command timing includes process startup/setup; structured case timings do not',
                        'RTT/cache labels describe caller setup, not automatic validation',
                        'bootstrap intervals are estimates, not proof of equivalence']}
    (args.output / 'comparison.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(summaries, indent=2))
    if args.max_regression_percent is not None:
        threshold = 1 / (1 + args.max_regression_percent / 100)
        operation_summaries = {k: v for k, v in summaries.items() if k != 'command_end_to_end'} or summaries
        if any(s['bootstrap_95_percent_interval'][1] < threshold for s in operation_summaries.values()):
            raise SystemExit('repeatable regression exceeds the requested threshold; see comparison.json')


if __name__ == '__main__':
    main()
