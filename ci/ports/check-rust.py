#!/usr/bin/env python3
"""Test an immutable Rust port against this platform, leaving caller checkouts alone."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


def use_development_vnfs(manifest, root):
    text, count = re.subn(
        r'(?m)^(vnfs\s*=\s*\{\s*)version\s*=\s*"[^"]+"',
        lambda match: match[1] + 'path = ' + json.dumps(str(root / 'crates/vnfs')),
        manifest.read_text(),
    )
    if count != 1:
        raise ValueError('expected exactly one inline vnfs dependency to retarget')
    manifest.write_text(text)


def main():
    repository, revision, port = sys.argv[1:]
    if port not in {'coreutils', 'findutils'} or not re.fullmatch(r'[0-9a-f]{40}', revision):
        raise ValueError('expected a supported port and full revision')
    root = Path.cwd().resolve()
    env = dict(os.environ, CARGO_TARGET_DIR=os.environ.get(
        'CARGO_TARGET_DIR', str(root / 'target/ports' / port)))
    with tempfile.TemporaryDirectory(prefix=f'vfsi-port-{port}-', dir=os.getenv('RUNNER_TEMP')) as tmp:
        checkout = Path(tmp)
        def run(*args, **kwargs):
            return subprocess.run(args, cwd=checkout, env=env, check=True, **kwargs)
        run('git', 'init', '--quiet')
        run('git', 'remote', 'add', 'origin', repository)
        run('git', 'fetch', '--quiet', '--depth=1', 'origin', revision)
        run('git', 'checkout', '--quiet', '--detach', 'FETCH_HEAD')
        use_development_vnfs(checkout / 'Cargo.toml', root)
        # Updating the disposable lockfile is necessary: pre-1.0 patch pins do
        # not admit the next platform version. Verify resolution, not just flags.
        metadata = json.loads(run('cargo', 'metadata', '--format-version=1',
                                  '--features', 'vnfs', capture_output=True).stdout)
        packages = [p for p in metadata['packages'] if p['name'] == 'vnfs']
        if len(packages) != 1 or Path(packages[0]['manifest_path']).resolve() != root / 'crates/vnfs/Cargo.toml':
            raise RuntimeError('port did not resolve the development vnfs')
        if port == 'coreutils':
            run('cargo', 'test', '--locked', '-p', 'uu_cp', '-p', 'uu_du', '-p', 'uu_ls',
                '-p', 'uucore', '--lib', '--features', 'vnfs')
            run('cargo', 'check', '--locked', '-p', 'uu_rm', '-p', 'uu_rmdir', '--features', 'vnfs')
            run('cargo', 'build', '--locked', '--no-default-features', '--features',
                'cp du ls rm rmdir feat_vnfs')
        else:
            run('cargo', 'test', '--locked', '--features', 'vnfs', '--lib')
            run('cargo', 'build', '--locked', '--features', 'vnfs')
            binary = Path(env['CARGO_TARGET_DIR']).resolve() / 'debug/find'
            fixture = checkout / 'fixture'
            (fixture / 'sub').mkdir(parents=True)
            (fixture / 'sub/file').write_text('data')
            (fixture / 'alias').symlink_to('sub')
            for expression in [['-printf', '%p %i %m %n %s\\n'], ['-maxdepth', '0'],
                               ['-depth'], ['-name', 'sub', '-prune', '-o', '-print'], ['-size', '4c']]:
                outputs = []
                for mode in ['off', 'dummy']:
                    result = subprocess.run([str(binary), str(fixture), *expression],
                        env=dict(env, LC_ALL='C', VNFS_IMPL=mode), capture_output=True, check=True)
                    outputs.append(sorted(result.stdout.splitlines()))
                if outputs[0] != outputs[1]:
                    raise RuntimeError(f'find CLI parity failed: {expression}')
    print(f'{port}: pinned source passed against development vnfs')


if __name__ == '__main__':
    main()
