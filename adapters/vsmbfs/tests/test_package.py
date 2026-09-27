"""Package boundary tests for the vsmbfs adapter."""

import importlib.util
import subprocess
import sys

import fsspec
import pytest
import vsmbfs


def test_registers_dedicated_fsspec_protocol():
    assert fsspec.get_filesystem_class("vsmbfs") is vsmbfs.VsmbFileSystem
    assert vsmbfs.VsmbFileSystem.protocol == "vsmbfs"


def test_rejects_non_smb_backend_before_connecting():
    with pytest.raises(ValueError, match="only supports backend='smb'"):
        vsmbfs.VsmbFileSystem(backend="nfs", share="data")


@pytest.mark.parametrize("order", ["nfs4fs,vsmbfs", "vsmbfs,nfs4fs"])
def test_both_protocols_register_blockcache_compat_in_either_import_order(order):
    if importlib.util.find_spec("nfs4fs") is None:
        pytest.skip("nfs4fs is not installed in this package-only environment")
    script = """
import importlib
import sys
from types import SimpleNamespace

for module in sys.argv[1].split(','):
    importlib.import_module(module)
from nfs4fs import Nfs4FileSystem
from vsmbfs import VsmbFileSystem
from vfsi_fsspec._blockcache import _is_compatible_backend

for backend_type in (Nfs4FileSystem, VsmbFileSystem):
    backend = object.__new__(backend_type)
    assert _is_compatible_backend(SimpleNamespace(fs=backend))
"""
    result = subprocess.run(
        [sys.executable, "-c", script, order],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr
