"""Error boundaries accept native exceptions without losing their diagnostics."""

import errno

import pytest
from vfsi_fsspec._fs import _errno, _oserror


@pytest.mark.parametrize(
    "error",
    [
        FileNotFoundError(errno.ENOENT, "not present"),
        OSError(errno.EBADMSG, "malformed response"),
        TimeoutError(errno.ETIMEDOUT, "deadline"),
        NotImplementedError("SMB DFS referrals are unsupported"),
    ],
)
def test_bulk_error_preserves_exception_identity_context_and_class(error):
    error.index = 7
    error.status_domain = "smb"
    error.status_code = 0xC0000257
    result = _oserror(error, "/file")
    assert result is error
    assert result.index == 7
    assert result.status_domain == "smb"
    assert result.status_code == 0xC0000257
    if isinstance(error, OSError):
        assert result.filename == "/file"
        assert _errno(result) == result.errno


def test_legacy_mock_errno_maps_still_work():
    assert isinstance(_oserror(errno.ENOENT, "/file"), FileNotFoundError)
    assert _errno(errno.ENOENT) == errno.ENOENT


def test_rebasing_error_preserves_identity_and_native_diagnostics():
    error = PermissionError(errno.EACCES, "server policy")
    error.index = 0
    error.status_domain = "smb"
    error.status_code = 0xC0000022
    assert _oserror(error, "/second", index=4) is error
    assert error.index == 4
    assert error.filename == "/second"
    assert error.status_domain == "smb"
    assert error.status_code == 0xC0000022
