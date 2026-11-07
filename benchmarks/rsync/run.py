#!/usr/bin/env python3
"""Frozen rsync sender-scan benchmark; fresh mounts, paired times, separate wire captures.

Requires the same Linux/sudo tools as benchmarks/du/run.py. Only owned network
namespaces receive netem. This deliberately reuses its capture/fixture helpers.
"""
import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import pwd
import re
import signal
import statistics
import subprocess
import sys
import tempfile
import tarfile
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'du'))
from generate import fingerprint
from run import CLIENT, SERVER, Link, capture_summary, checked, sha

HERE = Path(__file__).resolve().parent


def trace_entries(text):
    return sum(map(int, re.findall(r'^vfsi: listdirs completed entries=(\d+)$', text, re.M)))


def checked_trace(text, method):
    entries = trace_entries(text)
    if (method == 'vfsi') != (entries > 0):
        raise ValueError('acceleration/fallback trace contract failed')
    if method == 'vfsi' and any(not re.fullmatch(r'vfsi: listdirs completed entries=\d+', line) for line in text.splitlines() if line):
        raise ValueError('unexpected diagnostics: possible partial POSIX fallback')
    return entries


def validate_sample(row, method, expected_lines, parity):
    if row['stdout_lines'] != expected_lines or (parity and row['stdout_sha256'] != parity):
        raise ValueError('rsync output parity/count failed')
    if (method == 'vfsi') != (row['traced_entries'] > 0):
        raise ValueError('acceleration/fallback trace contract failed')
    return row['stdout_sha256']


def worker(args, trace_check=checked_trace):
    account = pwd.getpwnam(args.user)
    stem = args.output.resolve()
    checked(['mount', '--make-rprivate', '/'])
    with tempfile.TemporaryDirectory(prefix='vfsi-rsync-') as temp:
        mount, destination = Path(temp)/'mount', Path(temp)/'destination'
        mount.mkdir()
        destination.mkdir()
        os.chmod(temp, 0o755)
        os.chown(destination, account.pw_uid, account.pw_gid)
        checked(['mount', '-t', 'nfs4', '-o', 'vers=4.2,proto=tcp,sec=sys,nosharecache', SERVER+':'+args.export, mount])
        capture = process = None
        try:
            options = checked(['findmnt', '-n', '-o', 'OPTIONS', '--target', mount]).strip()
            os.chdir(mount)
            if args.capture:
                log = Path(str(stem)+'.capture.log')
                pcap = Path(str(stem)+'.pcap')
                with log.open('w') as stream:
                    capture = subprocess.Popen(['tcpdump', '--immediate-mode', '-n', '-B', '16384', '-i', args.interface,
                                                '-s', '0', '-w', str(pcap), 'tcp', 'port', '2049'],
                                               stdout=subprocess.DEVNULL, stderr=stream)
                deadline = time.monotonic()+10
                while 'listening on' not in log.read_text():
                    if capture.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError('capture failed to start')
                    time.sleep(.02)
            env = {k:v for k,v in os.environ.items() if not k.startswith(('VFSI_', 'VNFS_'))}
            env.update(LC_ALL='C', HOME=account.pw_dir, VFSI_TRACE='1',
                       VFSI_IMPL='off' if args.method == 'kernel' else 'nfs',
                       VFSI_LIBRARY='/nonexistent-vfsi-benchmark.so' if args.method == 'missing-library' else args.library)
            command = [args.binary, '-ani', '--out-format=%i %n%L']
            if args.method == 'unsupported':
                command += ['--hard-links']  # Intentionally outside port's supported scan subset.
            command += ['tree/', str(destination)+'/']
            stdout, stderr = Path(str(stem)+'.stdout'), Path(str(stem)+'.stderr')
            with stdout.open('wb') as out, stderr.open('wb') as err:
                start = time.perf_counter_ns()
                process = subprocess.Popen(command, env=env, stdout=out, stderr=err,
                    user=account.pw_uid, group=account.pw_gid,
                    extra_groups=os.getgrouplist(args.user, account.pw_gid), start_new_session=True)
                status = process.wait(timeout=args.timeout)
                elapsed = time.perf_counter_ns()-start
            if status:
                raise RuntimeError(f'rsync failed ({status}): {stderr.read_text()}')
            if any(destination.iterdir()):
                raise RuntimeError('dry-run unexpectedly modified destination')
            if capture:
                time.sleep(.15)
                capture.send_signal(signal.SIGINT)
                if capture.wait(timeout=10):
                    raise RuntimeError('capture process failed')
                capture = None
                dropped = re.search(r'(\d+) packets dropped by kernel', log.read_text())
                if not dropped or int(dropped[1]):
                    raise RuntimeError('missing/nonzero capture drop count')
            entries = trace_check(stderr.read_text(), args.method)
            output = stdout.read_bytes()
            result = dict(elapsed_ns=elapsed, stdout_sha256=hashlib.sha256(output).hexdigest(),
                          stdout_lines=len(output.splitlines()), traced_entries=entries, mount_options=options,
                          command=command)
            if args.capture:
                result['wire'] = capture_summary(pcap, stem)
            for path in (stdout, stderr):
                with gzip.open(str(path)+'.gz', 'wb') as target:
                    target.write(path.read_bytes())
                path.unlink()
            Path(str(stem)+'.json').write_text(json.dumps(result, indent=2)+'\n')
        finally:
            if process and process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            if capture:
                capture.send_signal(signal.SIGINT)
                capture.wait(timeout=10)
            os.chdir('/')
            checked(['umount', mount])


def build(checkout, output):
    baseline = json.loads((HERE/'baseline.json').read_text())
    if checked(['git', '-C', checkout, 'rev-parse', 'HEAD']).strip() != baseline['port_revision']:
        raise ValueError('checkout is not the frozen rsync port')
    if checked(['git', '-C', checkout, 'diff', 'HEAD', '--', '.', ':!VFSI.md']):
        raise ValueError('frozen port source has tracked edits')
    release = checkout/'build-vfsi-c'/f"vfsi-c-{baseline['vfsi_c']}-vnfs-{baseline['vnfs']}"
    with (output/'build.log').open('w') as log:
        if not (release/'source/Cargo.toml').is_file():
            raise ValueError('bootstrap the pinned SDK once with support/build-vfsi-c')
        if sha(release/'source/Cargo.toml') != baseline['sdk_manifest_sha256']:
            raise ValueError('SDK manifest differs from frozen release configuration')
        if sha(HERE/'Cargo.lock') != baseline['sdk_lock_sha256']:
            raise ValueError('checked-in SDK lock differs from frozen baseline')
        (release/'source/Cargo.lock').write_bytes((HERE/'Cargo.lock').read_bytes())
        if sha(release/f"vfsi-c-{baseline['vfsi_c']}.crate") != baseline['archive_sha256']:
            raise ValueError('published SDK archive differs from frozen checksum')
        # Verify the extracted Rust/C source against the checksum-verified archive.
        with tarfile.open(release/f"vfsi-c-{baseline['vfsi_c']}.crate") as archive:
            for member in archive.getmembers():
                path = Path(member.name)
                if not member.isfile() or path.name in ('Cargo.toml', 'Cargo.lock'):
                    continue
                relative = Path(*path.parts[1:])
                if relative.is_absolute() or '..' in relative.parts:
                    raise ValueError('unsafe archive source path')
                if (release/'source'/relative).read_bytes() != archive.extractfile(member).read():
                    raise ValueError('SDK source differs from published archive: '+str(relative))
        subprocess.run(['cargo','build','--release','--locked','--manifest-path',str(release/'source/Cargo.toml'),
                        '--target-dir',str(release/'target')], cwd=checkout, stdout=log, stderr=log, check=True, timeout=1800)
        metadata = json.loads(checked(['cargo', 'metadata', '--locked', '--format-version', '1', '--manifest-path', release/'source/Cargo.toml']))
        packages = metadata['packages']
        if any(p['source'] is None or not p['source'].startswith('registry+') for p in packages if p['id'] != metadata['resolve']['root']):
            raise ValueError('SDK has non-registry dependencies')
        for name, version in baseline['packages'].items():
            if next(p for p in packages if p['name']==name)['version'] != version:
                raise ValueError('SDK dependency drift: '+name)
        env = dict(os.environ, CPPFLAGS='-I'+str(release/'source/include'))
        subprocess.run(['./configure', '--enable-vfsi', '--disable-md2man', '--disable-xxhash', '--disable-zstd', '--disable-lz4', '--disable-idn'],
                       cwd=checkout, env=env, stdout=log, stderr=log, check=True, timeout=300)
        subprocess.run(['make', '-j2'], cwd=checkout, stdout=log, stderr=log, check=True, timeout=1200)
    with gzip.open(output/'build.log.gz', 'wb') as target:
        target.write((output/'build.log').read_bytes())
    (output/'build.log').unlink()
    binary, library = checkout/'rsync', release/'target/release/libvfsi_c.so'
    provenance = dict(baseline=baseline, binary_sha256=sha(binary), library_sha256=sha(library),
        sdk_lock_sha256=sha(release/'source/Cargo.lock'), packages={p['name']:dict(version=p['version'], source=p['source']) for p in packages},
        port_diff=checked(['git','-C',checkout,'diff','HEAD']),
        compiler=checked(['cc','--version']).splitlines()[0], rustc=checked(['rustc','-Vv']))
    return binary, library, provenance


def main(args, prepare=build, trial_script=None):
    if not (3 <= args.pairs <= 101 and 1 <= args.captures <= 9 and 0 < args.timeout <= 600):
        raise ValueError('need >=3 timing pairs, >=1 capture, and a bounded timeout')
    args.output.mkdir(parents=True, exist_ok=False)
    raw = args.output/'raw'
    raw.mkdir()
    binary, library, provenance = prepare(args.checkout.resolve(), args.output)
    fixture = json.loads((args.fixture/'fixture.json').read_text())
    expected = fingerprint(args.fixture/'tree')
    if expected['identity_sha256'] != fixture['identity_sha256']:
        raise ValueError('fixture differs from generated manifest')
    result = dict(schema_version=1, provenance=provenance, fixture=fixture, samples=[], regimes={},
                  environment=dict(uname=checked(['uname','-a']),server_sha256=sha('/usr/bin/ganesha.nfsd'),
                                   server_config_sha256=sha('/tmp/ganesha.conf')))
    parity = None
    with Link() as link:
        for regime, delay in [('native',0),('added-1ms-rtt',500)]:
            result['regimes'][regime] = link.delay(delay)
            phases = [('warmup',1),('timing',args.pairs),('capture',args.captures)]
            if regime == 'native':
                phases += [('fallback',1)]
            for phase, count in phases:
                for pair in range(count):
                    methods = ['missing-library','unsupported'] if phase == 'fallback' else (['kernel','vfsi'] if pair%2 == 0 else ['vfsi','kernel'])
                    for method in methods:
                        if fingerprint(args.fixture/'tree') != expected:
                            raise ValueError('fixture mutated')
                        stem = raw/f'{regime}-{phase}-{pair:02d}-{method}'
                        command = ['unshare','--mount','--fork',sys.executable,str(trial_script or Path(__file__).resolve()),'_trial',
                            '--binary',str(binary),'--library',str(library),'--export',args.export,'--method',method,
                            '--user',pwd.getpwuid(os.getuid()).pw_name,'--output',str(stem),'--interface',link.peer,'--timeout',str(args.timeout)]
                        if phase == 'capture':
                            command += ['--capture']
                        link.client(*command)
                        row = json.loads(Path(str(stem)+'.json').read_text())
                        parity = validate_sample(row, method, fixture['directories']+fixture['regular_files'], parity)
                        row.update(regime=regime,phase=phase,pair=pair,method=method,raw=stem.name)
                        result['samples'].append(row)
                        (args.output/'results.json').write_text(json.dumps(result,indent=2)+'\n')
                        print(stem.name, round(row['elapsed_ns']/1e6,3), row.get('wire',{}).get('rpc_calls'),flush=True)
    if fingerprint(args.fixture/'tree') != expected or sha(binary) != provenance['binary_sha256'] or sha(library) != provenance['library_sha256']:
        raise ValueError('fixture or binaries changed during measurement')
    result.update(output_parity=True, safe_fallback=True, summary=[])
    for regime in result['regimes']:
        for method in ('kernel','vfsi'):
            rows = [r for r in result['samples'] if r['regime']==regime and r['method']==method]
            result['summary'].append(dict(regime=regime,method=method,
                median_wall_ms=statistics.median(r['elapsed_ns']/1e6 for r in rows if r['phase']=='timing'),
                rpc_counts=[r['wire']['rpc_calls'] for r in rows if r['phase']=='capture']))
    (args.output/'results.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result['summary'],indent=2))


if __name__ == '__main__':
    signal.signal(signal.SIGTERM, lambda *unused: sys.exit('terminated'))
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command',required=True)
    run = sub.add_parser('run')
    for name in ('checkout','fixture','output'):
        run.add_argument('--'+name,type=Path,required=True)
    run.add_argument('--export',required=True)
    run.add_argument('--pairs',type=int,default=7)
    run.add_argument('--captures',type=int,default=3)
    run.add_argument('--timeout',type=float,default=120)
    trial = sub.add_parser('_trial')
    for name in ('binary','library','export','method','user','interface'):
        trial.add_argument('--'+name,required=True)
    trial.add_argument('--output',type=Path,required=True)
    trial.add_argument('--capture',action='store_true')
    trial.add_argument('--timeout',type=float,required=True)
    args = parser.parse_args()
    worker(args) if args.command == '_trial' else main(args)
