#!/usr/bin/env python3
"""Build an isolated baseline and compare paired complete-write batch timings."""

import argparse
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import shutil
import statistics
import subprocess
import tarfile
import time
import tomllib


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def build_configuration(root, environment, command, toolchain, cargo_version):
    """Conservatively fingerprint inputs that can change release code generation."""
    root = root.resolve()
    names = {'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'RUSTC', 'RUSTC_WRAPPER',
             'RUSTC_WORKSPACE_WRAPPER', 'RUSTUP_TOOLCHAIN', 'CARGO_INCREMENTAL'}
    overrides = {key: value for key, value in sorted(environment.items())
                 if key in names or key.startswith(('CARGO_PROFILE_', 'CARGO_BUILD_', 'CARGO_TARGET_'))
                 if key != 'CARGO_TARGET_DIR'}
    configs = []
    seen = set()

    def record(path):
        path = path.resolve()
        if path in seen:
            return
        seen.add(path)
        # Keep config contents (including credentials) out of the report.
        name = str(path.relative_to(root)) if path.is_relative_to(root) else str(path)
        configs.append({'path': name, 'sha256': sha256(path) if path.is_file() else None})
        if path.is_file():
            config = tomllib.loads(path.read_text())
            for include in config.get('include', []):
                include = include['path'] if isinstance(include, dict) else include
                record(path.parent / include)

    cargo_home = Path(environment.get('CARGO_HOME',
                                     str(Path(environment.get('HOME', str(Path.home()))) / '.cargo')))
    if not cargo_home.is_absolute():
        cargo_home = root / cargo_home
    directories = [root / '.cargo', *(parent / '.cargo' for parent in root.parents), cargo_home]
    for directory in directories:
        # Cargo prefers the legacy extensionless file when both exist.
        for name in ['config', 'config.toml']:
            path = directory / name
            if path.is_file():
                record(path)
                break
    profiles = tomllib.loads((root / 'Cargo.toml').read_text()).get('profile', {})
    return {'schema': 1, 'command': command, 'rustc': toolchain, 'cargo': cargo_version,
            'environment': overrides, 'workspace_profiles': profiles, 'cargo_configs': configs}


def validate_reuse_build(prior_build, expected_configuration):
    recorded = prior_build.get('build_configuration')
    if recorded is None:
        raise RuntimeError('saved baseline has no verified build configuration; '
                           'rebuild without --reuse-baseline')
    if recorded != expected_configuration:
        changed = [key for key in expected_configuration
                   if recorded.get(key) != expected_configuration[key]]
        raise RuntimeError('saved baseline build configuration differs (' + ', '.join(changed) +
                           '); rebuild without --reuse-baseline')


def compiler_profiles(log):
    """Read Cargo artifact profiles even when its incremental cache is fresh."""
    targets = {'vfsi_core', 'vfsi_sync', 'vfsi_local', 'vfsi_posix', 'vnfs',
               'write_schedule_benchmark'}
    profiles = {}
    for line in log.splitlines():
        if not line.startswith('{'):
            continue
        message = json.loads(line)
        if message.get('reason') == 'compiler-artifact':
            target = message['target']
            if target['name'] in targets and set(target['kind']) & {'lib', 'example'}:
                profiles[target['name']] = message['profile']
    if not {'vfsi_sync', 'write_schedule_benchmark'} <= profiles.keys():
        raise RuntimeError('build log is missing scheduler/example compiler artifact profiles')
    return profiles


def benchmark_executable(log):
    for line in log.splitlines():
        if not line.startswith('{'):
            continue
        message = json.loads(line)
        if (message.get('reason') == 'compiler-artifact'
                and message['target']['name'] == 'write_schedule_benchmark'
                and message.get('executable')):
            return Path(message['executable'])
    raise RuntimeError('build log is missing the benchmark executable artifact')


def percentile(values, quantile):
    values = sorted(values)
    at = (len(values) - 1) * quantile
    lower = math.floor(at)
    upper = math.ceil(at)
    return values[lower] + (values[upper] - values[lower]) * (at - lower)


def median_interval(values):
    rng = random.Random(104729)
    medians = [statistics.median(rng.choices(values, k=len(values))) for _ in range(10000)]
    return [percentile(medians, 0.025), percentile(medians, 0.975)]


def cases():
    selections = []
    for scenario, counts in [
        ('overlap', [1, 16, 64, 128, 256, 512, 1024]),
        ('chain', [64, 256, 1024]),
        ('scatter', [64, 256, 1024]),
        ('independent', [1, 16, 64, 256]),
        ('overlap-short', [64, 256]),
        ('independent-short', [64, 256]),
        ('mixed', [64, 256]),
        ('append', [64, 256]),
    ]:
        selections.extend(('sink', scenario, count) for count in counts)
    for scenario, counts in [
        ('overlap', [64, 256, 512]),
        ('chain', [64, 256]),
        ('scatter', [64, 256]),
        ('independent', [16, 64, 256]),
    ]:
        selections.extend(('posix', scenario, count) for count in counts)
    return selections


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', default='4b9ff87', help='baseline Git commit/ref')
    parser.add_argument('--samples', type=int, default=9, help='paired samples per case')
    parser.add_argument('--target-ms', type=float, default=200, help='target time for the slower sample')
    parser.add_argument('--cpu', type=int, help='allowed CPU used by both benchmark processes')
    parser.add_argument('--output', type=Path, help='directory for snapshots, binaries, logs and JSON')
    parser.add_argument('--filter', default='', help='substring matching backend/scenario/count')
    parser.add_argument('--case', action='append', dest='selected_cases', help='exact backend/scenario/count selection; repeatable')
    parser.add_argument('--reuse-baseline', type=Path, help='reuse a previous baseline executable')
    args = parser.parse_args()
    if args.samples < 5 or args.target_ms <= 0:
        parser.error('use at least five paired samples and a positive target time')
    repo = Path(__file__).resolve().parents[1]
    stamp = datetime.datetime.now(datetime.UTC).strftime('%Y%m%dT%H%M%S%fZ')
    output = (args.output or repo / 'target' / 'write-scheduler-benchmark' / stamp).resolve()
    output.mkdir(parents=True, exist_ok=False)
    baseline_ref = subprocess.check_output(
        ['git', 'rev-parse', '--verify', args.baseline + '^{commit}'], cwd=repo, text=True,
    ).strip()
    source = repo / 'crates/vfsi-sync/src/client/write.rs'
    source_hash = sha256(source)
    harness = Path('crates/vnfs/examples/write_schedule_benchmark.rs')
    toolchain = subprocess.check_output(['rustc', '-Vv'], text=True)
    cargo_version = subprocess.check_output(['cargo', '-V'], text=True)
    allowed = sorted(os.sched_getaffinity(0))
    cpu = allowed[-1] if args.cpu is None else args.cpu
    if cpu not in allowed:
        parser.error(f'CPU {cpu} is outside the allowed set {allowed}')
    environment = dict(os.environ)
    environment['CARGO_TARGET_DIR'] = environment.get('CARGO_TARGET_DIR', str(repo / 'target'))
    target = Path(environment['CARGO_TARGET_DIR']).resolve()
    build_args = [
        'cargo', 'build', '--release', '--locked', '--offline', '-p', 'vnfs',
        '--no-default-features', '--features', 'posix,test-faults',
        '--example', 'write_schedule_benchmark',
        '--message-format=json', '-vv',
    ]
    configuration = build_configuration(repo, environment, build_args, toolchain, cargo_version)
    builds = []

    def build(root, label):
        log = output / f'build-{label}.log'
        # Cargo's shared target cache can reuse a newer artifact from another
        # checkout whose relative source files have older timestamps. Keep
        # persistent per-version caches; never clean the user's normal cache.
        cache_name = 'baseline-' + baseline_ref[:12] if label == 'baseline' else 'optimized'
        build_target = target / 'write-scheduler-build' / cache_name
        build_environment = {**environment, 'CARGO_TARGET_DIR': str(build_target)}
        actual_configuration = build_configuration(root, build_environment, build_args,
                                                   toolchain, cargo_version)
        if actual_configuration != configuration:
            raise RuntimeError(f'{label} build configuration differs from the current workspace')
        started = time.perf_counter()
        print(f'Building {label} release binary...', flush=True)
        with log.open('w') as stream:
            result = subprocess.run(build_args, cwd=root, env=build_environment, stdout=stream,
                                    stderr=subprocess.STDOUT)
        if result.returncode:
            raise RuntimeError(f'{label} build failed; {log}\n' + '\n'.join(log.read_text().splitlines()[-80:]))
        build_log = log.read_text()
        profiles = compiler_profiles(build_log)
        executable = benchmark_executable(build_log)
        destination = output / label
        shutil.copy2(executable, destination)
        builds.append({'label': label, 'seconds': time.perf_counter() - started,
                       'command': build_args, 'target_dir': str(build_target),
                       'binary_sha256': sha256(destination), 'log': str(log),
                       'build_configuration': actual_configuration, 'compiler_profiles': profiles,
                       'artifact_executable': str(executable)})
        return destination

    if args.reuse_baseline:
        prior = json.loads((args.reuse_baseline.resolve().parent / 'results.json').read_text())
        for key, expected in [('isolated_build_caches', True), ('baseline_commit', baseline_ref),
                              ('harness_sha256', sha256(repo / harness)), ('rustc', toolchain)]:
            if prior['metadata'].get(key) != expected:
                raise RuntimeError(f'saved baseline {key} differs; rebuild without --reuse-baseline')
        prior_build = next(build for build in prior['metadata']['builds'] if build['label'] == 'baseline')
        validate_reuse_build(prior_build, configuration)
        if prior_build['binary_sha256'] != sha256(args.reuse_baseline.resolve()):
            raise RuntimeError('saved baseline executable hash differs')
        baseline_harness_hash = prior['metadata'].get('baseline_harness_sha256', prior['metadata']['harness_sha256'])
        baseline_binary = output / 'baseline'
        shutil.copy2(args.reuse_baseline.resolve(), baseline_binary)
        builds.append({**prior_build, 'reused_from': str(args.reuse_baseline.resolve())})
    else:
        snapshot = output / 'baseline-source'
        archive = output / 'baseline.tar'
        subprocess.run(['git', 'archive', baseline_ref, '-o', str(archive)], cwd=repo, check=True)
        with tarfile.open(archive) as tar:
            tar.extractall(snapshot, filter='data')
        archive.unlink()
        shutil.copy2(repo / harness, snapshot / harness)
        # Earlier backends group the same synthetic objects by their names and
        # have no optional object-reference hook. Remove only that method from
        # their harness; baseline implementation files remain unmodified.
        if 'fn open_object_ref' not in (snapshot / 'crates/vfsi-sync/src/backend/mod.rs').read_text():
            harness_source = (snapshot / harness).read_text()
            begin = harness_source.index('    // BEGIN OBJECT REFERENCE HOOK')
            end = harness_source.index('    // END OBJECT REFERENCE HOOK', begin)
            end = harness_source.index('\n', end) + 1
            (snapshot / harness).write_text(harness_source[:begin] + harness_source[end:])
        # The harness adds no dependencies. Its manifest entry is identical in
        # both trees; all baseline implementation files come from the commit.
        shutil.copy2(repo / 'crates/vnfs/Cargo.toml', snapshot / 'crates/vnfs/Cargo.toml')
        baseline_harness_hash = sha256(snapshot / harness)
        baseline_binary = build(snapshot, 'baseline')
    optimized_binary = build(repo, 'optimized')
    if builds[0].get('compiler_profiles') != builds[1]['compiler_profiles']:
        raise RuntimeError('baseline and optimized compiler artifact profiles differ; rebuild the baseline')
    if sha256(baseline_binary) == sha256(optimized_binary):
        raise RuntimeError('comparison binaries are identical')
    fixture_root = repo / 'target' / 'write-scheduler-fixtures'
    fixture_root.mkdir(exist_ok=True)
    metadata = {
        'started_utc': stamp,
        'baseline_commit': baseline_ref,
        'optimized_source_sha256': source_hash,
        'harness_sha256': sha256(repo / harness),
        'baseline_harness_sha256': baseline_harness_hash,
        'runner_sha256': sha256(Path(__file__)),
        'rustc': toolchain,
        'cargo': cargo_version,
        'platform': platform.platform(),
        'cpu': cpu,
        'allowed_cpus': allowed,
        'filesystem': subprocess.check_output(['df', '-T', str(fixture_root)], text=True),
        'builds': builds,
        'isolated_build_caches': True,
        'samples': args.samples,
        'target_ms': args.target_ms,
        'warmups': 3,
        'method': 'paired alternating AB/BA; pinned child CPU; median and percentile bootstrap of paired speedups; all samples retained',
        'io': 'buffered POSIX writes to reused extents; no fsync; opens/preparation/readback/cleanup excluded',
    }
    report = {'metadata': metadata, 'cases': []}
    report_path = output / 'results.json'
    report_path.write_text(json.dumps(report, indent=2) + '\n')

    def sample(binary, selection, iterations):
        backend, scenario, count = selection
        result = subprocess.run(
            ['taskset', '-c', str(cpu), str(binary), backend, scenario, str(count),
             str(iterations), '3', str(fixture_root)],
            text=True, capture_output=True,
        )
        if result.returncode:
            raise RuntimeError(f'benchmark failed: {result.args}\n{result.stdout}\n{result.stderr}')
        data = json.loads(result.stdout)
        assert (data['backend'], data['scenario'], data['count']) == selection
        return data

    for selection in cases():
        key = '/'.join(map(str, selection))
        if args.filter not in key or (args.selected_cases and key not in args.selected_cases):
            continue
        probes = [sample(binary, selection, 1) for binary in [baseline_binary, optimized_binary]]
        slow_ns = max(probe['ns_per_batch'] for probe in probes)
        iterations = max(1, min(5000000, int(args.target_ms * 1_000_000 / slow_ns)))
        pairs = []
        for index in range(args.samples):
            order = [('baseline', baseline_binary), ('optimized', optimized_binary)]
            if index % 2:
                order.reverse()
            pair = {'order': [label for label, _ in order]}
            for label, binary in order:
                pair[label] = sample(binary, selection, iterations)
            assert pair['baseline']['dispatches'] == pair['optimized']['dispatches'], 'backend wave count changed'
            pair['speedup'] = pair['baseline']['ns_per_batch'] / pair['optimized']['ns_per_batch']
            pairs.append(pair)
        speedups = [pair['speedup'] for pair in pairs]
        interval = median_interval(speedups)
        classification = ('improved' if interval[0] > 1.05 else
                          'regressed' if interval[1] < 0.95 else 'within_noise_or_small_change')
        entry = {
            'backend': selection[0], 'scenario': selection[1], 'count': selection[2],
            'iterations': iterations,
            'baseline_ns_median': statistics.median(pair['baseline']['ns_per_batch'] for pair in pairs),
            'optimized_ns_median': statistics.median(pair['optimized']['ns_per_batch'] for pair in pairs),
            'paired_speedup_median': statistics.median(speedups),
            'paired_speedup_ci95': interval,
            'classification': classification,
            'pairs': pairs,
        }
        report['cases'].append(entry)
        report_path.write_text(json.dumps(report, indent=2) + '\n')
        print(f'{key}: {entry["baseline_ns_median"] / 1000:.2f} -> '
              f'{entry["optimized_ns_median"] / 1000:.2f} us/batch; '
              f'{entry["paired_speedup_median"]:.3f}x '
              f'[{interval[0]:.3f}, {interval[1]:.3f}] {classification}', flush=True)
    if not report['cases']:
        raise RuntimeError('selection executed zero benchmark cases')
    if sha256(source) != source_hash:
        raise RuntimeError('optimized scheduler source changed during the benchmark')
    if build_configuration(repo, environment, build_args, toolchain, cargo_version) != configuration:
        raise RuntimeError('build configuration changed during the benchmark')
    report['metadata']['finished_utc'] = datetime.datetime.now(datetime.UTC).isoformat()
    report_path.write_text(json.dumps(report, indent=2) + '\n')
    print(f'Results: {report_path}', flush=True)


if __name__ == '__main__':
    main()
