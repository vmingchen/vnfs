"""Shared pytest fixtures for the nfs4fs test suite."""

import os
import uuid

import fsspec
import pytest

import nfs4fs  # noqa: F401  (registers the "nfs4" protocol)


@pytest.fixture
def dummy_fs(tmp_path):
    """A local-directory backend (no NFS server needed)."""
    fs = fsspec.filesystem("nfs4", backend="dummy", dummy_root=str(tmp_path / "root"))
    return fs


def _nfs_reachable(host="127.0.0.1"):
    from nfs4fs import _native

    try:
        _native.NfsClient(host, "nfs")
        return True
    except Exception:
        return False


@pytest.fixture
def nfs_fs():
    """An NFS-backed filesystem under the ubuntu-writable /export/git area."""
    if not _nfs_reachable():
        pytest.skip("local NFSv4.1 server (127.0.0.1) is not reachable")
    root = f"git/nfs4fs_it_{os.getpid()}_{uuid.uuid4().hex[:8]}"
    fs = fsspec.filesystem("nfs4", host="127.0.0.1", root=root)
    fs.mkdir("nfs4:///", create_parents=True)
    yield fs
    try:
        fs.rm("nfs4:///", recursive=True)
    except Exception:
        pass
