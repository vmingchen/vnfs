#!/usr/bin/env python3
"""Frozen du benchmark: uninstrumented paired timings plus separate RPC captures.

Requires Linux, sudo -n, iproute2, nfs-common, ping, tcpdump and tshark.
Only newly created veth interfaces/namespaces receive netem configuration.
"""
import argparse
from collections import Counter
import gzip
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import pwd
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time

from generate import fingerprint

HERE = Path(__file__).resolve().parent
SERVER, CLIENT = '169.254.241.1', '169.254.241.2'


def checked(args, **kwargs):
    kwargs.setdefault('timeout', 300)
    return subprocess.check_output([str(x) for x in args], text=True, **kwargs)


def sha(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def rpc_summary(lines):
    """Count decoded RPC PDUs, NOT packets; reject ambiguous field cardinality."""
    calls, replies, operations = Counter(), Counter(), Counter()
    procedures, rows, retransmissions = Counter(), [], 0
    for line in lines:
        columns = line.rstrip('\n').split('\t')
        if len(columns) != 10:
            raise ValueError('unexpected tshark field count')
        frame, src, sport, dport, kinds, xids, programs, procs, ops, retrans = columns
        types, ids = kinds.split(','), xids.split(',')
        if len(types) != len(ids) or len(set(types)) != 1:
            raise ValueError('ambiguous RPC PDU fields')
        retransmissions += bool(retrans)
        if types[0] == '0':
            ps, procedures_here = programs.split(','), procs.split(',')
            if src != CLIENT or dport != '2049' or len(ps) != len(ids) or len(procedures_here) != len(ids):
                raise ValueError('unexpected call direction/cardinality')
            if any(p != '100003' for p in ps):
                raise ValueError('unexpected RPC program')
            for xid, procedure in zip(ids, procedures_here):
                calls[(sport, xid)] += 1
                procedures[procedure] += 1
                rows.append([frame, sport, xid, procedure])
            if ops:
                operations.update(ops.split(','))
        elif types[0] == '1':
            if src != SERVER or sport != '2049':
                raise ValueError('unexpected reply direction')
            for xid in ids:
                replies[(dport, xid)] += 1
        else:
            raise ValueError('unknown RPC message type')
    if not calls or calls != replies:
        raise ValueError('capture has missing or unmatched RPC requests/replies')
    return {'rpc_calls': sum(calls.values()), 'unique_rpc_calls': len(calls),
            'repeated_xids': sum(calls.values()) - len(calls),
            'retransmitted_rpc_frames': retransmissions,
            'procedures': dict(procedures), 'request_opcodes': dict(operations)}, rows


def capture_summary(pcap, stem):
    fields = ['frame.number', 'ip.src', 'tcp.srcport', 'tcp.dstport', 'rpc.msgtyp',
              'rpc.xid', 'rpc.program', 'rpc.procedure', 'nfs.opcode', 'tcp.analysis.retransmission']
    # A privileged NFS client source port can itself be another well-known
    # service (e.g. 705/AgentX); never rely on heuristic port precedence.
    command = ['tshark', '-n', '-r', '-', '-d', 'tcp.port==2049,rpc', '-Y', 'rpc.msgtyp', '-T', 'fields',
               '-E', 'occurrence=a', '-E', 'aggregator=,']
    for field in fields:
        command += ['-e', field]
    # Opening here also supports distributions whose tshark AppArmor policy
    # disallows named captures under /home; no policy change is needed.
    with pcap.open('rb') as stream:
        try:
            decoded = checked(command, stdin=stream, stderr=subprocess.PIPE)
        except subprocess.CalledProcessError as error:
            raise RuntimeError('tshark failed: ' + error.stderr) from error
    summary, rows = rpc_summary(decoded.splitlines(keepends=True))
    with gzip.open(str(stem) + '.rpc-fields.tsv.gz', 'wt') as out:
        out.write('\t'.join(fields) + '\n' + decoded)
    summary['pcap_sha256'] = sha(pcap)
    with pcap.open('rb') as source, gzip.open(str(pcap) + '.gz', 'wb') as target:
        shutil.copyfileobj(source, target)
    pcap.unlink()
    return summary


def worker(args):
    """Runs as root inside private mount AND network namespaces."""
    account = pwd.getpwnam(args.user)
    stem = args.output.resolve()
    checked(['mount', '--make-rprivate', '/'])
    with tempfile.TemporaryDirectory(prefix='vfsi-du-mount-') as mounted:
        checked(['mount', '-t', 'nfs4', '-o',
                 'vers=4.2,proto=tcp,sec=sys,nosharecache', SERVER + ':' + args.export, mounted])
        capture = process = None
        try:
            mount_options = checked(['findmnt', '-n', '-o', 'OPTIONS', '--target', mounted]).strip()
            os.chdir(mounted)  # CWD preparation is outside timing/capture.
            if args.capture:
                capture_log = open(str(stem) + '.capture.log', 'w+')
                pcap = Path(str(stem) + '.pcap')
                # Immediate kernel delivery protects short commands; buffered
                # file writes and a bounded 16-MiB ring absorb scheduler stalls.
                capture = subprocess.Popen(['tcpdump', '--immediate-mode', '-n', '-B', '16384', '-i', args.interface,
                                            '-s', '0', '-w', str(pcap), 'tcp', 'port', '2049'],
                                           stdout=subprocess.DEVNULL, stderr=capture_log)
                deadline = time.monotonic() + 10
                while 'listening on' not in Path(capture_log.name).read_text():
                    if capture.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError('capture did not start')
                    time.sleep(.02)
            env = dict(os.environ, VNFS_IMPL='off' if args.method == 'kernel' else 'nfs',
                       VNFS_STATS='1', LC_ALL='C', HOME=account.pw_dir)
            for key in ['VNFS_PROFILE', 'VNFS_HOST', 'VNFS_ROOT', 'VNFS_MOUNT']:
                env.pop(key, None)
            stdout, stderr = Path(str(stem) + '.stdout'), Path(str(stem) + '.stderr')
            with stdout.open('wb') as out, stderr.open('wb') as err:
                start = time.perf_counter_ns()
                process = subprocess.Popen([args.binary, '-a', '-B1', 'tree'], env=env,
                                           stdout=out, stderr=err, user=account.pw_uid,
                                           group=account.pw_gid, extra_groups=os.getgrouplist(args.user, account.pw_gid),
                                           start_new_session=True)
                expired = threading.Event()
                def terminate():
                    expired.set()
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                timer = threading.Timer(args.timeout, terminate)
                timer.start()
                try:
                    status = process.wait()
                    elapsed = time.perf_counter_ns() - start
                finally:
                    timer.cancel()
                    timer.join()
            if status or expired.is_set():
                raise RuntimeError(f'du failed/timed out: {status}; see {stem}')
            if capture:
                time.sleep(.15)
                capture.send_signal(signal.SIGINT)
                if capture.wait(timeout=10) != 0:
                    raise RuntimeError('capture process failed')
                capture = None
                capture_log.close()
                log = Path(str(stem) + '.capture.log').read_text()
                dropped = re.search(r'(\d+) packets dropped by kernel', log)
                if not dropped or int(dropped[1]):
                    raise RuntimeError('capture drop count missing/nonzero')
            text = stderr.read_text()
            match = re.search(r'\[vnfs\] rpc_calls=(\d+)', text)
            if args.method == 'vfsi' and (not match or int(match[1]) <= 0):
                raise RuntimeError('VFSI telemetry absent: refusing to report silent fallback')
            lines = stdout.read_bytes().splitlines()
            result = {'elapsed_ns': elapsed, 'stdout_lines': len(lines),
                      'stdout_sorted_sha256': hashlib.sha256(b'\n'.join(sorted(lines)) + b'\n').hexdigest(),
                      'mount_options': mount_options,
                      'library_rpc_calls': int(match[1]) if match else None,
                      'library_diagnostics': text.strip()}
            if args.capture:
                result['wire'] = capture_summary(pcap, stem)
            for path in [stdout, stderr]:
                with path.open('rb') as source, gzip.open(str(path) + '.gz', 'wb') as target:
                    shutil.copyfileobj(source, target)
                path.unlink()
            Path(str(stem) + '.json').write_text(json.dumps(result, indent=2) + '\n')
        finally:
            if process and process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            if capture:
                capture.send_signal(signal.SIGINT)
                capture.wait(timeout=10)
            os.chdir('/')
            checked(['umount', mounted])


class Link:
    def __init__(self):
        suffix = f'{os.getpid():x}'
        self.ns, self.host, self.peer = 'vfsidu-' + suffix, 'vdh' + suffix, 'vdc' + suffix
        self.ns_created = self.link_created = False

    def privileged(self, *argv):
        return checked(['sudo', '-n', *argv])

    def client(self, *argv):
        return self.privileged('ip', 'netns', 'exec', self.ns, *argv)

    def __enter__(self):
        # Refuse to reuse any existing address on the benchmark subnet.
        subnet = ipaddress.ip_network(SERVER + '/30', strict=False)
        for route in json.loads(self.privileged('ip', '-j', '-4', 'route', 'show', 'table', 'all')):
            destination = route.get('dst', 'default')
            if destination != 'default' and subnet.overlaps(ipaddress.ip_network(destination, strict=False)):
                raise RuntimeError('benchmark subnet overlaps an existing route')
        try:
            self.privileged('ip', 'netns', 'add', self.ns)
            self.ns_created = True
            self.privileged('ip', 'link', 'add', self.host, 'type', 'veth', 'peer', 'name', self.peer)
            self.link_created = True
            self.privileged('ip', 'link', 'set', self.peer, 'netns', self.ns)
            self.privileged('ip', 'addr', 'add', SERVER + '/30', 'dev', self.host)
            self.privileged('ip', 'link', 'set', self.host, 'up')
            self.client('ip', 'addr', 'add', CLIENT + '/30', 'dev', self.peer)
            self.client('ip', 'link', 'set', self.peer, 'up')
            self.client('ip', 'link', 'set', 'lo', 'up')
            return self
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def __exit__(self, *unused):
        try:
            if self.link_created:
                self.privileged('ip', 'link', 'del', self.host)
        finally:
            if self.ns_created:
                self.privileged('ip', 'netns', 'del', self.ns)

    def delay(self, microseconds):
        for prefix, interface in [(['sudo', '-n'], self.host),
                                  (['sudo', '-n', 'ip', 'netns', 'exec', self.ns], self.peer)]:
            if microseconds:
                checked([*prefix, 'tc', 'qdisc', 'replace', 'dev', interface,
                         'root', 'netem', 'delay', str(microseconds) + 'us'])
        ping = self.client('ping', '-n', '-c', '9', '-i', '0.02', SERVER)
        times = [float(x) for x in re.findall(r'time=([0-9.]+) ms', ping)]
        if len(times) != 9:
            raise RuntimeError('ping calibration incomplete')
        return {'added_one_way_us': microseconds, 'ping_rtt_median_ms': statistics.median(times),
                'ping_raw': ping,
                'host_qdisc': json.loads(self.privileged('tc', '-s', '-j', 'qdisc', 'show', 'dev', self.host)),
                'client_qdisc': json.loads(self.client('tc', '-s', '-j', 'qdisc', 'show', 'dev', self.peer))}


def build(checkout, output):
    baseline = json.loads((HERE / 'baseline.json').read_text())
    if checked(['git', '-C', checkout, 'rev-parse', 'HEAD']).strip() != baseline['port_revision']:
        raise ValueError('checkout does not match frozen port revision')
    if checked(['git', '-C', checkout, 'diff', 'HEAD', '--']):
        raise ValueError('frozen checkout has tracked edits')
    if sha(Path(checkout) / 'Cargo.lock') != baseline['cargo_lock_sha256']:
        raise ValueError('Cargo.lock differs from frozen baseline')
    if sha(HERE / 'Cargo.lock') != baseline['cargo_lock_sha256']:
        raise ValueError('checked-in baseline lock changed')
    with (output / 'build.log').open('w') as log:
        subprocess.run(['cargo', *baseline['build_args']], cwd=checkout, stdout=log, stderr=log, check=True, timeout=1800)
    metadata = json.loads(checked(['cargo', 'metadata', '--locked', '--format-version', '1',
                                  '--manifest-path', str(Path(checkout) / 'src/uu/du/Cargo.toml'),
                                  '--features', 'vnfs'], cwd=checkout))
    packages = {p['name']: p for p in metadata['packages'] if p['name'] in baseline['packages']}
    for name, version in baseline['packages'].items():
        if name not in packages or packages[name]['version'] != version or not (packages[name]['source'] or '').startswith('registry+'):
            raise ValueError('dependency drift: ' + name)
    binary = Path(checkout).resolve() / 'target/release/du'
    return binary, {'baseline': baseline, 'binary_sha256': sha(binary),
                    'rustc': checked(['rustc', '-Vv']), 'cargo': checked(['cargo', '-V']),
                    'resolved_packages': {name: {'version': p['version'], 'source': p['source']}
                                          for name, p in packages.items()},
                    'build_environment': {k: v for k, v in os.environ.items()
                                          if k in ['RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_TARGET']}}


def main(args):
    if not (1 <= args.pairs <= 101 and 1 <= args.captures <= 9 and 0 < args.timeout <= 600):
        raise ValueError('invalid sample counts or timeout')
    args.output.mkdir(parents=True, exist_ok=False)
    raw = args.output / 'raw'
    raw.mkdir()
    binary, provenance = build(str(args.checkout.resolve()), args.output)
    fixture = json.loads((args.fixture / 'fixture.json').read_text())
    expected = fingerprint(args.fixture / 'tree')
    if expected['identity_sha256'] != fixture['identity_sha256']:
        raise ValueError('fixture changed after generation')
    result = {'schema_version': 1, 'provenance': provenance, 'fixture': fixture,
              'environment': {'uname': checked(['uname', '-a']),
                              'tshark': checked(['tshark', '--version']).splitlines()[0],
                              'server_binary_sha256': sha('/usr/bin/ganesha.nfsd'),
                              'server_version': checked(['/usr/bin/ganesha.nfsd', '-v'], stderr=subprocess.STDOUT).strip(),
                              'server_config_sha256': sha('/tmp/ganesha.conf'),
                              'cpu_count': os.cpu_count(), 'affinity': sorted(os.sched_getaffinity(0))},
              'samples': [], 'regimes': {}}
    parity = None
    with Link() as link:
        for label, delay in [('native', 0), ('added-1ms-rtt', 500)]:
            result['regimes'][label] = link.delay(delay)
            for phase, pairs in [('warmup', 1), ('timing', args.pairs), ('capture', args.captures)]:
                for pair in range(pairs):
                    methods = ['kernel', 'vfsi'] if pair % 2 == 0 else ['vfsi', 'kernel']
                    for method in methods:
                        if fingerprint(args.fixture / 'tree') != expected:
                            raise ValueError('fixture mutated')
                        name = f'{label}-{phase}-{pair:02d}-{method}'
                        stem = raw / name
                        command = ['unshare', '--mount', '--fork', sys.executable, str(Path(__file__).resolve()),
                                   '_trial', '--binary', str(binary), '--export', args.export,
                                   '--method', method, '--user', pwd.getpwuid(os.getuid()).pw_name,
                                   '--output', str(stem.resolve()), '--interface', link.peer,
                                   '--timeout', str(args.timeout)]
                        if phase == 'capture':
                            command += ['--capture']
                        link.client(*command)
                        row = json.loads(Path(str(stem) + '.json').read_text())
                        parity = parity or row['stdout_sorted_sha256']
                        if row['stdout_sorted_sha256'] != parity or row['stdout_lines'] != fixture['directories'] + fixture['regular_files']:
                            raise ValueError('du output parity failed: ' + name)
                        row.update(regime=label, phase=phase, pair=pair, method=method, raw=name)
                        result['samples'].append(row)
                        (args.output / 'results.json').write_text(json.dumps(result, indent=2) + '\n')
                        print(name, f'{row["elapsed_ns"]/1e6:.3f} ms', row.get('wire', {}).get('rpc_calls'), flush=True)
    result['output_parity'] = True
    result['stdout_sorted_sha256'] = parity
    result['summary'] = []
    for label in result['regimes']:
        medians = {}
        for method in ['kernel', 'vfsi']:
            rows = [r for r in result['samples'] if r['regime'] == label and r['method'] == method]
            wall = statistics.median(r['elapsed_ns']/1e6 for r in rows if r['phase'] == 'timing')
            rpcs = [r['wire']['rpc_calls'] for r in rows if r['phase'] == 'capture']
            medians[method] = wall
            result['summary'].append({'regime': label, 'method': method, 'median_wall_ms': wall,
                                      'median_rpc_calls': statistics.median(rpcs),
                                      'rpc_min': min(rpcs), 'rpc_max': max(rpcs)})
        result['regimes'][label]['speedup'] = medians['kernel'] / medians['vfsi']
    (args.output / 'results.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result['summary'], indent=2))


if __name__ == '__main__':
    def interrupted(signum, unused):
        raise KeyboardInterrupt(f'interrupted by signal {signum}')
    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    run = sub.add_parser('run')
    run.add_argument('--checkout', type=Path, required=True)
    run.add_argument('--fixture', type=Path, required=True)
    run.add_argument('--export', required=True, help='NFS path to fixture parent, e.g. /du-v027')
    run.add_argument('--output', type=Path, required=True)
    run.add_argument('--pairs', type=int, default=7)
    run.add_argument('--captures', type=int, default=3)
    run.add_argument('--timeout', type=float, default=120)
    trial = sub.add_parser('_trial')
    for name in ['binary', 'export', 'method', 'user', 'interface']:
        trial.add_argument('--' + name, required=True)
    trial.add_argument('--output', type=Path, required=True)
    trial.add_argument('--capture', action='store_true')
    trial.add_argument('--timeout', type=float, required=True)
    args = parser.parse_args()
    worker(args) if args.command == '_trial' else main(args)
