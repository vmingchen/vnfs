"""PRR round 2: reject invalid open-many modes before creating parents."""

import pytest
from fsspec.core import OpenFile


@pytest.mark.parametrize("invalid_mode", ["wat", "rwxq"])
def test_open_many_validates_all_modes_before_auto_mkdir(dummy_fs, invalid_mode):
    dummy_fs.auto_mkdir = True
    entries = [
        OpenFile(dummy_fs, "/valid-parent/file", "wb"),
        OpenFile(dummy_fs, "/invalid-parent/file", invalid_mode),
    ]
    with pytest.raises(ValueError, match="unsupported mode"):
        dummy_fs.open_many(entries)
    assert not dummy_fs.exists("/valid-parent")
    assert not dummy_fs.exists("/invalid-parent")
    assert not dummy_fs._client._fds
