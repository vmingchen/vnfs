"""PRR round 1: recover missing copy parents alongside other errors."""

import errno

import pytest
from fsspec.callbacks import Callback


@pytest.fixture(params=["dummy_fs", "nfs_fs"])
def fs(request):
    return request.getfixturevalue(request.param)


def test_copy_recovers_missing_parents_in_mixed_error_batch(fs, monkeypatch):
    fs.auto_mkdir = True
    fs.pipe({"/a": b"first", "/b": b"second", "/c": b"third"})
    original = fs._client.copy_many
    calls = []

    def mixed_result(pairs):
        calls.append(list(pairs))
        if len(calls) == 1:
            copied, errors = original(pairs[:2])
            copied.append(None)
            errors[2] = errno.EACCES
            return copied, errors
        return original(pairs)

    monkeypatch.setattr(fs._client, "copy_many", mixed_result)
    callback = Callback()
    with pytest.raises(PermissionError):
        fs.copy(
            ["/a", "/b", "/c"],
            ["/done", "/missing/parent/second", "/denied/third"],
            callback=callback,
        )

    assert fs.cat_file("/done") == b"first"
    assert fs.cat_file("/missing/parent/second") == b"second"
    assert not fs.exists("/denied/third")
    assert calls == [
        [
            (fs._native_path("/a"), fs._native_path("/done")),
            (fs._native_path("/b"), fs._native_path("/missing/parent/second")),
            (fs._native_path("/c"), fs._native_path("/denied/third")),
        ],
        [
            (fs._native_path("/b"), fs._native_path("/missing/parent/second")),
        ],
    ]
    assert callback.size == callback.value == 3
    assert not fs._client._fds
