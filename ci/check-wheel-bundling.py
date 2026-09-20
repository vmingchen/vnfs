#!/usr/bin/env python3
"""Verify a repaired wheel bundles the native libraries NFS needs at runtime.

``maturin build --compatibility pypi`` runs auditwheel, which copies
non-platform shared libraries into ``<package>.libs/`` and rewrites RPATH. If
that step silently drops a library, the wheel only imports on a host that
happens to provide it. Fail the release instead of shipping such a wheel.

Usage: check-wheel-bundling.py <wheel> [wheel...]
"""

import sys
import zipfile

# Library-name stems that must be present in the wheel's bundled libraries.
# auditwheel may mangle the filename (for example ``libntirpc-<hash>.so.6.3``),
# so match on a substring rather than an exact name.
REQUIRED = ("libntirpc", "libgssapi_krb5", "liburcu")


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
    missing = [stem for stem in REQUIRED if not any(stem in name for name in basenames)]
    for stem in missing:
        print(f"ERROR: {wheel} does not bundle {stem}*", file=sys.stderr)
    return not missing


def main(argv):
    if not argv:
        sys.exit("usage: check-wheel-bundling.py <wheel> [wheel...]")
    if not all(check(wheel) for wheel in argv):
        sys.exit(1)


if __name__ == "__main__":
    main(sys.argv[1:])
