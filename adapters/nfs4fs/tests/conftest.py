"""Shared pytest fixtures for the nfs4fs test suite."""

import os
import uuid

import fsspec
import nfs4fs  # noqa: F401  (registers the "nfs4" protocol)
import pytest

from ._servers import nfs_config, nfs_reachable


def _required(name):
    return os.environ.get(name) == "1"


@pytest.fixture
def dummy_fs(tmp_path):
    """A local-directory backend (no NFS server needed)."""
    fs = fsspec.filesystem("nfs4", backend="dummy", dummy_root=str(tmp_path / "root"))
    yield fs
    fs.close()


@pytest.fixture
def nfs_fs():
    """An NFS-backed filesystem under the ubuntu-writable /export/git area."""
    host, minor_version = nfs_config()
    if not nfs_reachable(host, minor_version):
        if _required("VFSI_NFS_REQUIRED"):
            pytest.fail(f"required NFS server {host!r} is not reachable")
        pytest.skip(f"NFS server {host!r} is not reachable")
    root = f"git/nfs4fs_it_{os.getpid()}_{uuid.uuid4().hex[:8]}"
    fs = fsspec.filesystem("nfs4", host=host, root=root, minor_version=minor_version)
    fs.mkdir("nfs4:///", create_parents=True)
    yield fs
    try:
        fs.rm("nfs4:///", recursive=True)
    except Exception:
        pass
    fs.close()
