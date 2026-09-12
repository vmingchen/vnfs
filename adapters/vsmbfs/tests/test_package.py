"""Package boundary tests for the vsmbfs adapter."""

import fsspec
import pytest
import vsmbfs


def test_registers_dedicated_fsspec_protocol():
    assert fsspec.get_filesystem_class("vsmbfs") is vsmbfs.VsmbFileSystem
    assert vsmbfs.VsmbFileSystem.protocol == "vsmbfs"


def test_rejects_non_smb_backend_before_connecting():
    with pytest.raises(ValueError, match="only supports backend='smb'"):
        vsmbfs.VsmbFileSystem(backend="nfs", share="data")
