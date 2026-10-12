"""Production-review regressions for staging ownership and file lifecycles."""

import pytest


@pytest.fixture(params=["dummy_fs", "nfs_fs"])
def fs(request):
    return request.getfixturevalue(request.param)


@pytest.mark.parametrize("mode", ["wb", "xb"])
@pytest.mark.parametrize("kind", ["file", "symlink"])
@pytest.mark.parametrize("failure", ["collision", "disconnect"])
def test_failed_stage_open_never_removes_unowned_path(
    fs, monkeypatch, mode, kind, failure
):
    file = fs.open("/destination", mode, autocommit=False)
    file.write(b"transaction payload")
    fs.pipe_file("/competitor", b"keep competitor")
    if kind == "file":
        fs.pipe_file(file.temp_path, b"keep staging name")
    else:
        fs.symlink("competitor", file.temp_path)
    native_open = fs._client.open

    def disconnect(path, mode, **kwargs):
        if path == fs._native_path(file.temp_path):
            raise ConnectionError("unacknowledged stage open")
        return native_open(path, mode, **kwargs)

    if failure == "disconnect":
        monkeypatch.setattr(fs._client, "open", disconnect)
    try:
        with pytest.raises(
            ConnectionError if failure == "disconnect" else FileExistsError
        ):
            file.commit()
        file.discard()
        if kind == "file":
            assert fs.cat_file(file.temp_path) == b"keep staging name"
        else:
            assert fs.readlink(file.temp_path) == "competitor"
        assert fs.cat_file("/competitor") == b"keep competitor"
        assert not fs.exists("/destination")
        assert not fs._client._fds
    finally:
        file.discard()
