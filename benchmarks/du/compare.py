#!/usr/bin/env python3
"""Compare an explicit development du binary with the frozen release.

The frozen runner/lock remain unchanged. Candidate source diffs, lock and
binary hashes are retained separately; never interpret this as a release bump.
"""
import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import pwd
import signal
import statistics
import subprocess
import sys

import run
from generate import fingerprint


def postorder(stem):
    with gzip.open(str(stem)+'.stdout.gz', 'rb') as stream:
        positions = {line.rstrip(b'\n').split(b'\t', 1)[1]: i
                     for i, line in enumerate(stream)}
    for path, position in positions.items():
        parent = path.rpartition(b'/')[0]
        if parent in positions and positions[parent] <= position:
            raise ValueError('directory precedes its descendant: '+repr(path))


def build_candidate(checkout, output):
    """Rebuild outside timing; use Cargo's artifact path, never a stale guess."""
    checkout = checkout.resolve()
    baseline = json.loads((run.HERE/'baseline.json').read_text())
    command = ['cargo', *baseline['build_args'], '--message-format=json-render-diagnostics']
    artifacts = output/'candidate-build.jsonl'
    with artifacts.open('w') as messages, (output/'candidate-build.log').open('w') as log:
        subprocess.run(command, cwd=checkout, stdout=messages, stderr=log,
                       check=True, timeout=1800)
    executables = set()
    for line in artifacts.read_text().splitlines():
        record = json.loads(line)
        target = record.get('target', {})
        if (record.get('reason') == 'compiler-artifact' and target.get('name') == 'du'
                and 'bin' in target.get('kind', []) and record.get('executable')):
            executable = Path(record['executable'])
            executables.add((checkout/executable).resolve())
    if len(executables) != 1:
        raise RuntimeError('candidate build did not report exactly one du executable')
    binary = executables.pop()
    return binary, dict(command=command, executable=str(binary), binary_sha256=run.sha(binary),
                        rustc=run.checked(['rustc', '-Vv'], cwd=checkout),
                        cargo=run.checked(['cargo', '-V'], cwd=checkout))


def main(args):
    if not (1 <= args.pairs <= 101 and 1 <= args.captures <= 9):
        raise ValueError('invalid sample counts')
    args.output.mkdir(parents=True, exist_ok=False)
    raw = args.output/'raw'
    raw.mkdir()
    frozen, provenance = run.build(str(args.frozen.resolve()), args.output)
    published = json.loads(args.baseline_results.read_text())
    if provenance['binary_sha256'] != published['provenance']['binary_sha256']:
        raise ValueError('frozen binary differs from published baseline')
    candidate, candidate_build = build_candidate(args.candidate, args.output)
    candidate_hash = candidate_build['binary_sha256']
    source = run.checked(['git','-C', args.candidate,'diff','--binary','HEAD'])
    library = run.checked(['git','-C', args.library,'diff','--binary','HEAD','--','crates'])
    (args.output/'candidate-port.patch').write_text(source)
    (args.output/'candidate-library.patch').write_text(library)
    (args.output/'candidate-Cargo.lock').write_bytes((args.candidate/'Cargo.lock').read_bytes())
    fixture = json.loads((args.fixture/'fixture.json').read_text())
    expected = fingerprint(args.fixture/'tree')
    if expected['identity_sha256'] != fixture['identity_sha256']:
        raise ValueError('fixture identity changed')
    result = dict(schema_version=1, frozen=provenance, fixture=fixture, samples=[], regimes={},
                  candidate=dict(binary_sha256=candidate_hash, build=candidate_build,
                    port_head=run.checked(['git','-C',args.candidate,'rev-parse','HEAD']).strip(),
                    library_head=run.checked(['git','-C',args.library,'rev-parse','HEAD']).strip(),
                    port_patch_sha256=hashlib.sha256(source.encode()).hexdigest(),
                    library_patch_sha256=hashlib.sha256(library.encode()).hexdigest(),
                    lock_sha256=run.sha(args.candidate/'Cargo.lock')))
    parity = None
    cases = [('kernel',frozen,'kernel'), ('frozen-vfsi',frozen,'vfsi'), ('candidate-vfsi',candidate,'vfsi')]
    with run.Link() as link:
        for regime, delay in [('native',0), ('added-1ms-rtt',500)]:
            result['regimes'][regime] = link.delay(delay)
            for phase, count in [('warmup',1),('timing',args.pairs),('capture',args.captures)]:
                for pair in range(count):
                    ordered = cases[pair % 3:]+cases[:pair % 3]
                    if pair % 2:
                        ordered = list(reversed(ordered))
                    for label,binary,method in ordered:
                        if fingerprint(args.fixture/'tree') != expected:
                            raise ValueError('fixture mutated')
                        stem=raw/f'{regime}-{phase}-{pair:02d}-{label}'
                        command=['unshare','--mount','--fork',sys.executable,str(run.HERE/'run.py'),
                                 '_trial','--binary',str(binary),'--export',args.export,'--method',method,
                                 '--user',pwd.getpwuid(os.getuid()).pw_name,'--output',str(stem.resolve()),
                                 '--interface',link.peer,'--timeout','120']
                        if phase == 'capture':
                            command += ['--capture']
                        link.client(*command)
                        row=json.loads(Path(str(stem)+'.json').read_text())
                        parity=parity or row['stdout_sorted_sha256']
                        if row['stdout_sorted_sha256'] != parity or row['stdout_lines'] != fixture['directories']+fixture['regular_files']:
                            raise ValueError('output parity failure')
                        postorder(stem)
                        row.update(regime=regime,phase=phase,pair=pair,variant=label,raw=stem.name)
                        result['samples'].append(row)
                        (args.output/'results.json').write_text(json.dumps(result,indent=2)+'\n')
                        print(stem.name,round(row['elapsed_ns']/1e6,3),row.get('wire',{}).get('rpc_calls'),flush=True)
    assert run.sha(candidate)==candidate_hash
    assert source==run.checked(['git','-C',args.candidate,'diff','--binary','HEAD'])
    assert library==run.checked(['git','-C',args.library,'diff','--binary','HEAD','--','crates'])
    assert fingerprint(args.fixture/'tree')==expected
    result['output_parity']=result['post_order']=True
    result['summary']=[]
    for regime in result['regimes']:
        for label,_,_ in cases:
            rows=[r for r in result['samples'] if r['regime']==regime and r['variant']==label]
            captures=[r['wire']['rpc_calls'] for r in rows if r['phase']=='capture']
            result['summary'].append(dict(regime=regime,variant=label,
                median_wall_ms=statistics.median(r['elapsed_ns']/1e6 for r in rows if r['phase']=='timing'),
                median_rpc_calls=statistics.median(captures), rpc_min=min(captures),rpc_max=max(captures)))
    (args.output/'results.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result['summary'],indent=2),flush=True)


if __name__=='__main__':
    def interrupted(signum, unused):
        raise KeyboardInterrupt(f'interrupted by signal {signum}')
    signal.signal(signal.SIGTERM, interrupted)
    p=argparse.ArgumentParser(description=__doc__)
    for name in ['frozen','candidate','library','fixture','output']:
        p.add_argument('--'+name,type=Path,required=True)
    p.add_argument('--export',required=True)
    p.add_argument('--baseline-results',type=Path,required=True)
    p.add_argument('--pairs',type=int,default=7)
    p.add_argument('--captures',type=int,default=3)
    main(p.parse_args())
