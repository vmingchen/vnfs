"""Shared pytest fixtures for the nfs4fs test suite."""

import os
import uuid

import fsspec
import pytest

import nfs4fs  # noqa: F401  (registers the "nfs4" protocol)


def _required(name):
    return os.environ.get(name) == "1"


@pytest.fixture
def dummy_fs(tmp_path):
    """A local-directory backend (no NFS server needed)."""
    fs = fsspec.filesystem("nfs4", backend="dummy", dummy_root=str(tmp_path / "root"))
    return fs


def _nfs_config():
    host = os.environ.get("VFSI_NFS_SERVER", "127.0.0.1")
    value = os.environ.get("VFSI_NFS_MINOR")
    return host, int(value) if value else None


def _nfs_reachable(host, minor_version):
    from nfs4fs import _native

    try:
        _native.NfsClient(host, minor_version=minor_version)
        return True
    except Exception:
        return False


@pytest.fixture
def nfs_fs():
    """An NFS-backed filesystem under the ubuntu-writable /export/git area."""
    host, minor_version = _nfs_config()
    if not _nfs_reachable(host, minor_version):
        if _required("VFSI_NFS_REQUIRED"):
            pytest.fail(f"required NFS server {host!r} is not reachable")
        pytest.skip("local NFSv4.1 server (127.0.0.1) is not reachable")
    root = f"git/nfs4fs_it_{os.getpid()}_{uuid.uuid4().hex[:8]}"
    fs = fsspec.filesystem("nfs4", host=host, root=root, minor_version=minor_version)
    fs.mkdir("nfs4:///", create_parents=True)
    yield fs
    try:
        fs.rm("nfs4:///", recursive=True)
    except Exception:
        pass


@pytest.fixture
def smb_fs():
    """An SMB-backed filesystem when a test share is configured."""
    server = os.environ.get("VFSI_SMB_SERVER")
    share = os.environ.get("VFSI_SMB_SHARE")
    if not server or not share:
        if _required("VFSI_SMB_REQUIRED"):
            pytest.fail(
                "VFSI_SMB_SERVER and VFSI_SMB_SHARE are required in this integration job"
            )
        pytest.skip("VFSI_SMB_SERVER and VFSI_SMB_SHARE are not configured")
    root = f"vfsi-python-it-{os.getpid()}-{uuid.uuid4().hex[:8]}"
    fs = fsspec.filesystem(
        "nfs4",
        host=server,
        backend="smb",
        share=share,
        username=os.environ.get("VFSI_SMB_USERNAME", ""),
        password=os.environ.get("VFSI_SMB_PASSWORD", ""),
        domain=os.environ.get("VFSI_SMB_DOMAIN", ""),
        root=root,
    )
    fs.mkdir("nfs4:///", create_parents=True)
    yield fs
    try:
        fs.rm("nfs4:///", recursive=True)
    except Exception:
        pass
