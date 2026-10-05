#!/usr/bin/env python3
"""Verify a repaired wheel bundles the native libraries NFS needs at runtime.

``maturin build --compatibility pypi`` runs auditwheel, which copies
non-platform shared libraries into ``<package>.libs/`` and rewrites RPATH. If
that step silently drops a library, the wheel only imports on a host that
happens to provide it. Fail the release instead of shipping such a wheel.

Usage: check-wheel-bundling.py <wheel> [wheel...]
"""

import sys
import re
import zipfile
import subprocess
import tempfile
from pathlib import Path

# Library-name stems that must be present in the wheel's bundled libraries.
# auditwheel may mangle the filename,
# so match on a substring rather than an exact name.
REQUIRED = ("libgssapi_krb5",)


def dynamic_dependencies(wheel):
    """Inspect ELF dependency tags, not symbol names or archive substrings."""
    dependencies = set()
    with zipfile.ZipFile(wheel) as archive, tempfile.TemporaryDirectory() as temp:
        for index, name in enumerate(archive.namelist()):
            basename = name.rsplit("/", 1)[-1]
            if not (basename.endswith(".so") or ".so." in basename):
                continue
            # Do not extract an archive-supplied path into the filesystem.
            path = Path(temp) / f"library-{index}.so"
            path.write_bytes(archive.read(name))
            result = subprocess.run(
                ["readelf", "--dynamic", str(path)],
                capture_output=True,
                text=True,
                check=True,
            )
            dependencies.update(
                re.findall(r"\(NEEDED\).*\[([^\]]+)\]", result.stdout)
            )
    return dependencies


def bundled_libraries(wheel):
    with zipfile.ZipFile(wheel) as archive:
        return [
            name
            for name in archive.namelist()
            if ".libs/" in name and not name.endswith("/")
        ]


def check(wheel):
    bundled = bundled_libraries(wheel)
    basenames = [name.rsplit("/", 1)[-1] for name in bundled]
    print(f"{wheel}: {len(bundled)} bundled libraries")
    for name in sorted(basenames):
        print(f"  {name}")
    dependencies = dynamic_dependencies(wheel)
    # Static ntirpc builds can eliminate their unused RCU dependency. Require
    # RCU only when an ELF object actually needs it; importing on a build host
    # alone does not prove a dynamically needed library was bundled.
    missing = [stem for stem in REQUIRED if not any(stem in name for name in basenames)]
    missing.extend(
        sorted(name for name in dependencies if name.startswith("liburcu") and name not in basenames)
    )
    for stem in missing:
        print(f"ERROR: {wheel} does not bundle {stem}*", file=sys.stderr)
    unexpected = any("libntirpc" in name for name in basenames)
    if unexpected:
        print(f"ERROR: {wheel} bundles a second libntirpc", file=sys.stderr)
    dynamic_ntirpc = any("libntirpc" in name for name in dependencies)
    if dynamic_ntirpc:
        print(f"ERROR: {wheel} dynamically links libntirpc", file=sys.stderr)
    return not missing and not unexpected and not dynamic_ntirpc


def main(argv):
    if not argv:
        sys.exit("usage: check-wheel-bundling.py <wheel> [wheel...]")
    if not all(check(wheel) for wheel in argv):
        sys.exit(1)


if __name__ == "__main__":
    main(sys.argv[1:])
