"""Production-review regressions for mutation failure and recovery paths."""

import errno
import os

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
    pread_many = fs._client.preadv
    calls = []

    def short_scalar(fd, length, offset):
        return pread(fd, min(length, 2), offset)

    def short_vector(fds, offsets, lengths):
        calls.append(len(fds))
        return pread_many(fds, offsets, [min(length, 2) for length in lengths])

    monkeypatch.setattr(fs._client, "pread", short_scalar)
    monkeypatch.setattr(fs._client, "preadv", short_vector)
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


@pytest.mark.parametrize("cache", ["readahead", "blockcache", "mmap"])
@pytest.mark.parametrize("whole", [False, True])
def test_completed_read_does_not_drain_short_speculative_sibling(
    fs, monkeypatch, cache, whole
):
    from fsspec.core import OpenFile

    fs.block_size = 128
    fs.cache_type = cache
    fs.pipe({"/current": b"a" * 128, "/sibling": b"b" * 128})
    files = fs.open_many(
        [OpenFile(fs, path, "rb") for path in ["/current", "/sibling"]]
    )
    current, sibling = files
    sibling_fd = sibling._fd
    native_read = fs._client.preadv
    requests = []

    def short_sibling(fds, offsets, lengths):
        requests.append(tuple(fds))
        # The requested file completes immediately. Only the speculative
        # sibling produces valid positive short reads, never false EOF.
        return native_read(
            fds,
            offsets,
            [
                min(length, 1) if fd == sibling_fd else length
                for fd, length in zip(fds, lengths)
            ],
        )

    monkeypatch.setattr(fs._client, "preadv", short_sibling)
    try:
        assert current.read(-1 if whole else 1) == b"a" * (128 if whole else 1)
        assert len(requests) == 1, "completed read waited for speculative-only I/O"
        assert requests[0] == (current._fd, sibling_fd)  # Keep initial fan-out.
        # A fix must not mark a short speculative prefix as a complete block.
        assert sibling.read(1) == b"b"
        assert sibling.read() == b"b" * 127
    finally:
        fs.commit_many(files)
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
    original = fs._client.preadv
    calls = []

    def partial_then_failure(fds, offsets, lengths):
        calls.append((len(fds), list(offsets), list(lengths)))
        if len(calls) == 1:
            return original(fds, offsets, [2, 8, 2])
        if len(calls) == 2:
            if outcome == "reconnect":
                raise ConnectionError("injected continuation disconnect")
            results = original(fds, offsets, lengths)
            results[1] = (
                OSError(errno.EIO, "injected continuation failure")
                if outcome == "error"
                else b""
            )
            return results
        return original(fds, offsets, lengths)

    monkeypatch.setattr(fs._client, "preadv", partial_then_failure)
    try:
        data, errors = files[0]._buffer_group._pread_many(files, [0, 0, 0], [8, 8, 8])
        assert data[:2] == [payload, payload]
        if outcome == "error":
            assert data[2] is None
            assert set(errors) == {2}
            assert errors[2].errno == errno.EIO
        else:
            assert errors == {}
            assert data[2] == (payload[:2] if outcome == "eof" else payload)
        assert calls[:2] == [(3, [0, 0, 0], [8, 8, 8]), (2, [2, 2], [6, 6])]
        assert len(calls) == (3 if outcome == "reconnect" else 2)
    finally:
        fs.commit_many(files)
    assert not fs._client._fds


@pytest.mark.parametrize("failure", ["write", "callback", "interrupt"])
def test_exclusive_failure_never_unlinks_replacement_file(fs, monkeypatch, failure):
    original_write = fs._client.pwrite
    replaced = False
    error = (
        KeyboardInterrupt()
        if failure == "interrupt"
        else OSError(errno.ENOSPC, "injected failure")
    )

    def replace_destination():
        nonlocal replaced
        if not replaced:
            replaced = True
            fs.mv("/exclusive", "/original-inode")
            fs.pipe_file("/exclusive", b"other writer")

    def interrupted_write(fd, data, offset):
        written = original_write(fd, data, offset)
        if failure != "callback":
            replace_destination()
            raise error
        return written

    class ReplacingCallback(Callback):
        def relative_update(self, inc=1):
            replace_destination()
            raise error

    monkeypatch.setattr(fs._client, "pwrite", interrupted_write)
    callback = ReplacingCallback() if failure == "callback" else None
    with pytest.raises(type(error)) as caught:
        fs.pipe_file("/exclusive", b"original data", mode="create", callback=callback)
    assert caught.value is error
    assert fs.cat_file("/exclusive") == b"other writer"
    assert fs.cat_file("/original-inode") == b"original data"
    assert not fs._client._fds


def test_failed_explicit_group_flush_preserves_healthy_sibling(dummy_fs, monkeypatch):
    from fsspec.core import OpenFile

    fs = dummy_fs
    files = fs.open_many(
        [OpenFile(fs, "/failed", mode="wb"), OpenFile(fs, "/healthy", mode="wb")],
        write_buffering=True,
    )
    first, second = files
    first.write(b"discard")
    second.write(b"retain")
    pwritev = fs._client.pwritev

    def fail_first(fds, offsets, datas):
        if first._raw._fd in fds:
            assert fds == [first._raw._fd]
            return [OSError(errno.ENOSPC, "disk full")]
        return pwritev(fds, offsets, datas)

    monkeypatch.setattr(fs._client, "pwritev", fail_first)
    with pytest.raises(OSError) as flush_error:
        first.flush()
    assert flush_error.value.errno == errno.ENOSPC
    with pytest.raises(OSError) as close_error:
        fs.commit_many(files)
    assert close_error.value.errno == errno.ENOSPC
    assert close_error.value.filename == "/failed"
    assert fs.cat_file("/failed") == b""
    assert fs.cat_file("/healthy") == b"retain"
    assert not fs._client._fds


@pytest.mark.parametrize("method", ["read", "seek"])
def test_grouped_spool_failure_preserves_healthy_sibling(dummy_fs, monkeypatch, method):
    from fsspec.core import OpenFile

    fs = dummy_fs
    files = fs.open_many(
        [OpenFile(fs, "/failed", mode="wb"), OpenFile(fs, "/healthy", mode="wb")],
        write_buffering=True,
    )
    first, second = files
    first.write(b"discard")
    second.write(b"retain")
    first._stage_buffer()
    second._stage_buffer()
    original = getattr(first._write_spool, method)

    def fail_spool(*args):
        if method == "seek" and len(args) == 2 and args[1] == os.SEEK_END:
            return original(*args)
        raise OSError(errno.EIO, "spool error")

    monkeypatch.setattr(first._write_spool, method, fail_spool)
    with pytest.raises(OSError) as failure:
        fs.commit_many(files)
    assert failure.value.errno == errno.EIO
    assert failure.value.filename == "/failed"
    assert failure.value.index == 0
    assert fs.cat_file("/failed") == b""
    assert fs.cat_file("/healthy") == b"retain"
    assert not fs._client._fds


@pytest.mark.parametrize("whole", [False, True])
def test_speculative_read_timeout_keeps_the_writer_session(
    dummy_fs, monkeypatch, whole
):
    from fsspec.core import OpenFile

    fs = dummy_fs
    fs.block_size = 128
    fs.pipe({"/current": b"a" * 128, "/sibling": b"b" * 128})
    files = fs.open_many(
        [OpenFile(fs, path, "rb") for path in ["/current", "/sibling"]]
    )
    current, sibling = files
    expected_fds = (current._fd, sibling._fd)
    pool = fs._client
    generation = pool.generation
    original = pool.preadv
    calls = []

    def timed_out_sibling(fds, offsets, lengths):
        calls.append(tuple(fds))
        outcomes = original(fds, offsets, lengths)
        if len(calls) == 1:
            outcomes[1] = TimeoutError(errno.ETIMEDOUT, "speculative read timed out")
        return outcomes

    monkeypatch.setattr(pool, "preadv", timed_out_sibling)
    try:
        with fs.open("/writer", "wb") as writer:
            assert current.read(-1 if whole else 1) == b"a" * (128 if whole else 1)
            assert calls == [expected_fds]
            assert pool.generation == generation
            writer.write(b"finished")
        assert sibling.read() == b"b" * 128
        assert fs.cat_file("/writer") == b"finished"
    finally:
        fs.commit_many(files)
