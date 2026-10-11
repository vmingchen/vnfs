"""Production-review regressions for mutation failure and recovery paths."""

import errno

import pytest
from fsspec.callbacks import Callback


@pytest.fixture(params=["dummy_fs", "nfs_fs"])
def fs(request):
    return request.getfixturevalue(request.param)


@pytest.mark.parametrize("retry_fails", [False, True])
def test_copy_parent_recovery_does_not_replay_successful_pairs(
    fs, monkeypatch, retry_fails
):
    fs.auto_mkdir = True
    fs.pipe({"/a": b"original-a", "/b": b"original-b", "/source": b"third"})
    original = fs._client.copy_many
    calls = []

    def record(pairs):
        calls.append(list(pairs))
        if len(calls) == 2 and retry_fails:
            return [None] * len(pairs), {len(pairs) - 1: errno.EACCES}
        return original(pairs)

    monkeypatch.setattr(fs._client, "copy_many", record)
    callback = Callback()
    sources = ["/a", "/b", "/source"]
    destinations = ["/b", "/c", "/missing/d"]
    if retry_fails:
        with pytest.raises(PermissionError) as error:
            fs.copy(sources, destinations, callback=callback)
        assert error.value.filename == "/source"
    else:
        fs.copy(sources, destinations, callback=callback)
        assert fs.cat_file("/missing/d") == b"third"
    assert fs.cat_file("/b") == b"original-a"
    assert fs.cat_file("/c") == b"original-b"
    assert [len(batch) for batch in calls] == [3, 1]
    assert calls[1] == [(fs._native_path("/source"), fs._native_path("/missing/d"))]
    assert callback.value == callback.size == 3
