"""Integration coverage for the Python/fsspec SMB backend."""

from nfs4fs import _native

from .common import run_correctness_suite


def test_correctness_suite_on_smb(smb_fs):
    run_correctness_suite(smb_fs)


def test_smb_identity_and_capabilities(smb_fs):
    assert 0x0202 <= smb_fs.smb_dialect() <= 0x0311
    assert smb_fs._client.minor_version() is None
    assert smb_fs._client.capabilities() & _native.CAP_LSTAT == 0
