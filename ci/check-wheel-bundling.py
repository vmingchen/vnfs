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

# auditwheel can rename the library while retaining its stem.
GSS_LIBRARY = "libgssapi_krb5"


def dynamic_dependencies(archive):
    """Inspect ELF dependency tags, not symbol names or archive substrings."""
    dependencies = set()
    with tempfile.TemporaryDirectory() as temp:
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


def check(wheel):
    with zipfile.ZipFile(wheel) as archive:
        names = archive.namelist()
        if not any(re.fullmatch(r"nfs4fs/_native(?:\.[^/]+)?\.so", name) for name in names):
            print(f"ERROR: {wheel} does not contain the nfs4fs native extension", file=sys.stderr)
            return False
        # auditwheel adds nfs4fs.libs itself to the ELF search path.
        basenames = {
            name.split("/", 1)[1]
            for name in names
            if name.startswith("nfs4fs.libs/")
            and name.count("/") == 1
            and not name.endswith("/")
        }
        dependencies = dynamic_dependencies(archive)
    print(f"{wheel}: {len(basenames)} bundled libraries")
    for name in sorted(basenames):
        print(f"  {name}")
    # Match the exact ELF dependency name after auditwheel rewriting. A
    # bundled file with the same library stem may have a different SONAME.
    # Static builds can eliminate RCU, so require it only when referenced.
    missing = {
        name
        for name in dependencies
        if name.startswith((GSS_LIBRARY, "liburcu")) and name not in basenames
    }
    if not any(name.startswith(GSS_LIBRARY) for name in basenames | missing):
        missing.add(f"{GSS_LIBRARY}*")
    for name in sorted(missing):
        print(f"ERROR: {wheel} does not bundle {name}", file=sys.stderr)
    unexpected = any(
        ".libs/" in name and "libntirpc" in name.rsplit("/", 1)[-1] for name in names
    )
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
