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


@pytest.mark.parametrize("cache", ["readahead", "blockcache", "mmap"])
@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize("whole", [False, True])
def test_cached_reads_complete_short_native_results(
    fs, monkeypatch, cache, grouped, whole
):
    from fsspec.core import OpenFile, OpenFiles

    payload = b"abcdefghijklmnop"
    fs.pipe({"/short": payload, "/sibling": payload[::-1]})
    pread = fs._client.pread
    pread_many = fs._client.pread_many
    calls = []

    def short_scalar(fd, length, offset):
        return pread(fd, min(length, 2), offset)

    def short_vector(fds, offsets, lengths):
        calls.append(len(fds))
        return pread_many(fds, offsets, [min(length, 2) for length in lengths])

    monkeypatch.setattr(fs._client, "pread", short_scalar)
    monkeypatch.setattr(fs._client, "pread_many", short_vector)
    fs.block_size = 4
    fs.cache_type = cache
    context = (
        OpenFiles(
            [OpenFile(fs, p, "rb") for p in ["/short", "/sibling"]], mode="rb", fs=fs
        )
        if grouped
        else fs.open("/short", "rb")
    )
    with context as opened:
        files = opened if grouped else [opened]
        for file, expected in zip(files, [payload, payload[::-1]]):
            if whole:
                assert file.read() == expected
            else:
                assert file.read(8) == expected[:8]
                assert file.read(8) == expected[8:]
            file.seek(0)
            assert file.read(3) == expected[:3]
    if grouped:
        assert 2 in calls  # Partial results must retain the vector path.
    assert not fs._client._fds


@pytest.mark.parametrize("outcome", ["eof", "error", "reconnect"])
def test_grouped_short_read_continuation_preserves_completed_results(
    fs, monkeypatch, outcome
):
    from fsspec.core import OpenFile

    payload = b"abcdefgh"
    paths = ["/first", "/complete", "/last"]
    fs.pipe(dict.fromkeys(paths, payload))
    files = fs.open_many([OpenFile(fs, path, "rb") for path in paths])
    original = fs._client.pread_many
    calls = []

    def partial_then_failure(fds, offsets, lengths):
        calls.append((len(fds), list(offsets), list(lengths)))
        if len(calls) == 1:
            return original(fds, offsets, [2, 8, 2])
        if len(calls) == 2:
            if outcome == "reconnect":
                raise ConnectionError("injected continuation disconnect")
            data, errors = original(fds, offsets, lengths)
            data[1] = None if outcome == "error" else b""
            if outcome == "error":
                errors[1] = errno.EIO
            return data, errors
        return original(fds, offsets, lengths)

    monkeypatch.setattr(fs._client, "pread_many", partial_then_failure)
    try:
        data, errors = files[0]._buffer_group._pread_many(files, [0, 0, 0], [8, 8, 8])
        assert data[:2] == [payload, payload]
        if outcome == "error":
            assert data[2] is None
            assert errors == {2: errno.EIO}
        else:
            assert errors == {}
            assert data[2] == (payload[:2] if outcome == "eof" else payload)
        assert calls[:2] == [(3, [0, 0, 0], [8, 8, 8]), (2, [2, 2], [6, 6])]
        assert len(calls) == (3 if outcome == "reconnect" else 2)
    finally:
        fs.commit_many(files)
    assert not fs._client._fds
