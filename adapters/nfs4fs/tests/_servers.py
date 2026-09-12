"""Shared server configuration helpers for integration tests."""

import os
from functools import lru_cache


def nfs_config():
    host = os.environ.get("VFSI_NFS_SERVER", "127.0.0.1")
    value = os.environ.get("VFSI_NFS_MINOR")
    return host, int(value) if value else None


@lru_cache(maxsize=None)
def nfs_reachable(host, minor_version):
    """Probe each configured NFS endpoint at most once per pytest process."""
    from nfs4fs import _native

    try:
        client = _native.NfsClient(host, minor_version=minor_version)
        client.shutdown()
        return True
    except Exception:
        return False
