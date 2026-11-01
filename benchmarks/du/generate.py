#!/usr/bin/env python3
"""Generate a bounded, deterministic metadata tree in a NEW directory."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import stat


def geometry(branches, depth, files, size):
    if not (1 <= branches <= 16 and 0 <= depth <= 8 and 1 <= files <= 128 and 0 <= size <= 65536):
        raise ValueError('geometry outside supported bounds')
    dirs = sum(branches ** n for n in range(depth + 1))
    if dirs * (files + 1) > 100000 or dirs * files * size > 256 * 1024 * 1024:
        raise ValueError('fixture exceeds 100,000 entries or 256 MiB of payload')
    return dirs, dirs * files


def fingerprint(tree):
    digest = hashlib.sha256()
    counts = {'directories': 0, 'regular_files': 0, 'logical_file_bytes': 0,
              'allocated_bytes': 0, 'symlinks': 0, 'hard_link_aliases': 0}
    for parent, dirs, files in os.walk(tree):
        dirs.sort()
        files.sort()
        for path in [Path(parent), *(Path(parent) / name for name in files)]:
            st = path.lstat()
            if not (stat.S_ISDIR(st.st_mode) or stat.S_ISREG(st.st_mode)):
                raise ValueError('unexpected fixture object: ' + str(path))
            kind = 'directories' if stat.S_ISDIR(st.st_mode) else 'regular_files'
            counts[kind] += 1
            counts['allocated_bytes'] += st.st_blocks * 512
            if kind == 'regular_files':
                counts['logical_file_bytes'] += st.st_size
            row = [str(path.relative_to(tree)), st.st_mode, st.st_size, st.st_blocks,
                   st.st_nlink, st.st_mtime_ns, st.st_dev, st.st_ino, st.st_uid, st.st_gid]
            digest.update((json.dumps(row, separators=(',', ':')) + '\n').encode())
    return {'identity_sha256': digest.hexdigest(), **counts}


def generate(root, branches=6, depth=4, files=8, size=128):
    directories, regular_files = geometry(branches, depth, files, size)
    root.mkdir()  # Refuse to overwrite any existing fixture.
    tree = root / 'tree'
    unit = b'VFSI du metadata benchmark\n'
    payload = (unit * ((size + len(unit) - 1) // len(unit)))[:size]
    def fill(path, level):
        path.mkdir(mode=0o755)
        path.chmod(0o755)
        for index in range(files):
            with (path / f'f{index:03d}-metadata-small-file').open('xb') as stream:
                stream.write(payload)
                stream.flush()
                os.fsync(stream.fileno())
            Path(stream.name).chmod(0o644)
            os.utime(stream.name, ns=(1700000000000000000, 1700000000000000000))
        if level < depth:
            for index in range(branches):
                fill(path / f'd{index:02d}', level + 1)
        os.utime(path, ns=(1700000000000000000, 1700000000000000000))
    fill(tree, 0)
    observed = fingerprint(tree)
    assert observed['directories'] == directories and observed['regular_files'] == regular_files
    manifest = {'schema_version': 1, 'branches': branches, 'depth': depth,
                'files_per_directory': files, 'file_bytes': size,
                'payload_sha256': hashlib.sha256(payload).hexdigest(), **observed}
    (root / 'fixture.json').write_text(json.dumps(manifest, indent=2) + '\n')
    return manifest


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('root', type=Path)
    for name, value in [('branches', 6), ('depth', 4), ('files', 8), ('size', 128)]:
        parser.add_argument('--' + name, type=int, default=value)
    args = parser.parse_args()
    print(json.dumps(generate(args.root, args.branches, args.depth, args.files, args.size), indent=2))
