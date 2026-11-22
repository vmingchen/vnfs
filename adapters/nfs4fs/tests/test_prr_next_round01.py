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


def test_copy_parent_transport_failure_is_not_replayed_or_ignored(
    dummy_fs, monkeypatch
):
    dummy_fs.auto_mkdir = True
    dummy_fs.pipe_file("/b", b"second")
    copies = []
    parents = []

    def missing_parent(pairs):
        copies.append(list(pairs))
        return [None, None], {0: errno.EACCES, 1: errno.ENOENT}

    def disconnected(paths, mode=None):
        parents.append(list(paths))
        raise ConnectionError("parent transport failure")

    monkeypatch.setattr(dummy_fs._client, "copy_many", missing_parent)
    monkeypatch.setattr(dummy_fs, "_ensure_dirs", disconnected)
    with pytest.raises(ConnectionError, match="parent transport failure"):
        dummy_fs.copy(["/a", "/b"], ["/target/a", "/target/b"], on_error="ignore")
    assert len(copies) == len(parents) == 1


@pytest.mark.parametrize("on_error", ["raise", "ignore"])
def test_copy_parent_failure_preserves_policy_and_recovers_other_siblings(
    fs, monkeypatch, on_error
):
    fs.auto_mkdir = True
    fs.pipe({"/a": b"first", "/b": b"second", "/c": b"third", "/blocked": b"keep"})
    pairs = [
        ("/a", "/denied/a"),
        ("/b", "/blocked/parent/b"),
        ("/c", "/recoverable/parent/c"),
    ]
    original = fs._client.copy_many
    calls = []

    def mixed_result(native_pairs):
        calls.append(list(native_pairs))
        if len(calls) == 1:
            return [None] * 3, {0: errno.EACCES, 1: errno.ENOENT, 2: errno.ENOENT}
        return original(native_pairs)

    monkeypatch.setattr(fs._client, "copy_many", mixed_result)
    callback = Callback()
    if on_error == "raise":
        with pytest.raises(PermissionError) as error:
            fs.copy(*map(list, zip(*pairs)), on_error=on_error, callback=callback)
        assert error.value.errno == errno.EACCES
        assert error.value.filename == "/a"
    else:
        fs.copy(*map(list, zip(*pairs)), on_error=on_error, callback=callback)

    assert calls == [
        [(fs._native_path(a), fs._native_path(b)) for a, b in pairs],
        [(fs._native_path("/c"), fs._native_path("/recoverable/parent/c"))],
    ]
    assert fs.cat_file("/recoverable/parent/c") == b"third"
    assert fs.cat_file("/blocked") == b"keep"
    assert not fs.exists("/denied/a")
    assert callback.size == callback.value == 3
    assert not fs._client._fds


@pytest.mark.parametrize("on_error", ["raise", "ignore"])
def test_missing_copy_source_does_not_create_destination_parents(dummy_fs, on_error):
    dummy_fs.auto_mkdir = True
    dummy_fs.pipe_file("/good", b"contents")
    callback = Callback()
    sources = ["/missing", "/good"]
    destinations = ["/unneeded/child", "/needed/child"]
    if on_error == "raise":
        with pytest.raises(FileNotFoundError) as error:
            dummy_fs.copy(sources, destinations, on_error=on_error, callback=callback)
        assert error.value.index == 0
        assert error.value.filename == "/missing"
    else:
        dummy_fs.copy(sources, destinations, on_error=on_error, callback=callback)
    assert not dummy_fs.exists("/unneeded")
    assert dummy_fs.cat_file("/needed/child") == b"contents"
    assert callback.size == callback.value == 2
