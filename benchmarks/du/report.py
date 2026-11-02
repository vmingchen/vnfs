#!/usr/bin/env python3
"""Verify retained raw evidence and regenerate the concise results table."""
import argparse
import gzip
import hashlib
import json
from pathlib import Path
import re
import statistics

from run import rpc_summary


def verify(path):
    report = json.loads(path.read_text())
    if report.get('output_parity') is not True:
        raise ValueError('experiment did not complete')
    raw, keys = path.parent / 'raw', set()
    for sample in report['samples']:
        method = sample.get('variant', sample.get('method'))
        key = tuple(sample[k] for k in ('regime', 'phase', 'pair')) + (method,)
        if key in keys:
            raise ValueError('duplicate sample')
        keys.add(key)
        stem = raw / sample['raw']
        data = gzip.decompress(Path(str(stem) + '.stdout.gz').read_bytes())
        lines = data.splitlines()
        digest = hashlib.sha256(b'\n'.join(sorted(lines)) + b'\n').hexdigest()
        expected = report.get('stdout_sorted_sha256', report['samples'][0]['stdout_sorted_sha256'])
        if digest != expected or len(lines) != report['fixture']['directories'] + report['fixture']['regular_files']:
            raise ValueError('raw output parity failed')
        stderr = gzip.decompress(Path(str(stem) + '.stderr.gz').read_bytes()).decode()
        if method != 'kernel' and not re.search(r'\[vnfs\] rpc_calls=[1-9][0-9]*', stderr):
            raise ValueError('VFSI path not demonstrated')
        if 'candidate' in report:
            from compare import postorder
            postorder(stem)
        if sample['phase'] == 'capture':
            log = Path(str(stem) + '.capture.log').read_text()
            drops = re.search(r'(\d+) packets dropped by kernel', log)
            if not drops or int(drops[1]):
                raise ValueError('invalid capture drop count')
            with gzip.open(str(stem) + '.rpc-fields.tsv.gz', 'rt') as stream:
                next(stream)
                counters, _ = rpc_summary(stream)
            if any(counters[k] != sample['wire'][k] for k in counters):
                raise ValueError('raw RPC counts differ from summary')
            with gzip.open(str(stem) + '.pcap.gz', 'rb') as stream:
                if hashlib.file_digest(stream, 'sha256').hexdigest() != sample['wire']['pcap_sha256']:
                    raise ValueError('pcap digest differs')
    if 'candidate' in report:
        for filename, field in [('candidate-port.patch', 'port_patch_sha256'),
                                ('candidate-library.patch', 'library_patch_sha256'),
                                ('candidate-Cargo.lock', 'lock_sha256')]:
            if hashlib.sha256((path.parent/filename).read_bytes()).hexdigest() != report['candidate'][field]:
                raise ValueError('candidate provenance differs: '+filename)
    return report


def table(report):
    if 'candidate' in report:
        rows = ['| Network | Kernel, ms | Frozen VFSI, ms | Candidate, ms | vs kernel | vs frozen VFSI | RPCs: kernel / frozen / candidate |',
                '|---|---:|---:|---:|---:|---:|---|']
        for regime in report['regimes']:
            values = {}
            for variant in ['kernel', 'frozen-vfsi', 'candidate-vfsi']:
                samples = [s for s in report['samples'] if s['regime']==regime and s['variant']==variant]
                timings = [s['elapsed_ns']/1e6 for s in samples if s['phase']=='timing']
                counts = [s['wire']['rpc_calls'] for s in samples if s['phase']=='capture']
                if len(timings) < 3 or not counts:
                    raise ValueError('insufficient publishable samples')
                values[variant] = statistics.median(timings), statistics.median(counts)
            kernel, frozen, candidate = (values[v] for v in ['kernel', 'frozen-vfsi', 'candidate-vfsi'])
            rows.append(f'| {regime} | {kernel[0]:.3f} | {frozen[0]:.3f} | {candidate[0]:.3f} | '
                        f'{kernel[0]/candidate[0]:.2f}× | {frozen[0]/candidate[0]:.2f}× | '
                        f'{kernel[1]:,.0f} / {frozen[1]:,.0f} / {candidate[1]:,.0f} |')
        return '\n'.join(rows)
    rows = ['| Network | Measured RTT, ms | Kernel median, ms | VFSI median, ms | Speedup | Kernel RPCs | VFSI RPCs | Parity |',
            '|---|---:|---:|---:|---:|---:|---:|---|']
    for regime, network in report['regimes'].items():
        values = {}
        for method in ['kernel', 'vfsi']:
            samples = [s for s in report['samples'] if s['regime'] == regime and s['method'] == method]
            timings = [s['elapsed_ns']/1e6 for s in samples if s['phase'] == 'timing']
            counts = [s['wire']['rpc_calls'] for s in samples if s['phase'] == 'capture']
            if len(timings) < 3 or not counts:
                raise ValueError('insufficient publishable samples')
            values[method] = statistics.median(timings), statistics.median(counts)
        kernel, vfsi = values['kernel'], values['vfsi']
        rows.append(f'| {regime} | {network["ping_rtt_median_ms"]:.3f} | {kernel[0]:.3f} | {vfsi[0]:.3f} | '
                    f'{kernel[0]/vfsi[0]:.2f}× | {kernel[1]:,.0f} | {vfsi[1]:,.0f} | Identical |')
    return '\n'.join(rows)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('results', type=Path)
    args = parser.parse_args()
    print(table(verify(args.results)))
