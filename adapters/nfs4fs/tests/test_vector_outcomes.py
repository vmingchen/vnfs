"""Independent outcomes retain healthy work and never replay failed writes."""

import errno
from fsspec.core import OpenFile, OpenFiles
import pytest


def opened(fs, paths, mode):
    return OpenFiles(
        [OpenFile(fs, path, mode=mode) for path in paths], mode=mode, fs=fs
    )


def failure(prefix=b"", *, eof=False):
    error = ConnectionError("lost read")
    error.domain = "transport"
    error.transport_kind = "connection"
    error.failure_state = "unknown"
    error.read_prefix = prefix
    error.read_offset = 0
    error.read_eof = eof
    return error


@pytest.mark.parametrize("recovery_fails", [False, True])
def test_group_reads_retry_only_failed_suffixes(dummy_fs, monkeypatch, recovery_fails):
    fs = dummy_fs
    fs.pipe({"/a": b"abcd", "/b": b"efgh"})
    with opened(fs, ["/a", "/b"], "rb") as files:
        group = files[0]._buffer_group
        calls = []
        first_error = failure(b"e")

        def read(fds, offsets, lengths):
            calls.append((list(offsets), list(lengths)))
            if len(calls) == 1:
                return [b"abcd", first_error]
            return [b"fgh"]

        def recover():
            if recovery_fails:
                raise ConnectionError("reconnect failed")

        monkeypatch.setattr(fs._client, "preadv", read)
        monkeypatch.setattr(group, "_reopen_readers", recover)
        data, errors = group._pread_many_once(files, [0, 0], [4, 4])
        assert data[0] == b"abcd"
        if recovery_fails:
            assert data[1] is None and errors[1] is first_error
            assert len(calls) == 1
        else:
            assert data == [b"abcd", b"efgh"] and not errors
            assert calls == [([0, 0], [4, 4]), ([1], [3])]


@pytest.mark.parametrize("eof", [False, True])
def test_complete_failed_prefix_is_not_reported_as_success(dummy_fs, monkeypatch, eof):
    fs = dummy_fs
    fs.pipe_file("/a", b"abcd")
    with opened(fs, ["/a"], "rb") as files:
        group = files[0]._buffer_group
        error = failure(b"abcd", eof=eof)
        monkeypatch.setattr(fs._client, "preadv", lambda *args: [error])
        monkeypatch.setattr(
            group, "_reopen_readers", lambda: pytest.fail("must not replay")
        )
        data, errors = group._pread_many_once(files, [0], [4])
        assert data == [None] and errors == {0: error}


@pytest.mark.parametrize("mode", ["wb", "ab"])
def test_group_write_failure_keeps_healthy_siblings_and_later_waves(
    dummy_fs, monkeypatch, mode
):
    fs = dummy_fs
    fs.write_buffering = True
    fs.block_size = 2
    fs.batch_size = 8
    fs.max_batch_bytes = 8
    fs.pipe({"/a": b"", "/b": b""})
    method = "appendv" if mode == "ab" else "pwritev"
    original = getattr(fs._client, method)
    calls = []
    error = OSError(errno.EIO, "partial failure")
    error.failure_state = "unknown"
    error.bytes_written = 1

    def write(fds, *args):
        calls.append(len(fds))
        if len(calls) == 1:
            # Submit just the healthy input, then emulate a sibling failure.
            healthy_args = [[values[0]] for values in args]
            healthy = original([fds[0]], *healthy_args)
            return [healthy[0], error]
        return original(fds, *args)

    monkeypatch.setattr(fs._client, method, write)
    with pytest.raises(OSError) as caught:
        with opened(fs, ["/a", "/b"], mode) as files:
            files[0].write(b"abcdef")
            files[1].write(b"uvwxyz")
    assert caught.value is error
    assert fs.cat_file("/a") == b"abcdef"
    assert fs.cat_file("/b") == b""
    assert calls[0] == 2 and all(count == 1 for count in calls[1:])


@pytest.mark.parametrize("mode", ["wb", "ab"])
def test_grouped_io_preserves_old_native_fallback(dummy_fs, monkeypatch, mode):
    fs = dummy_fs
    fs.write_buffering = True
    fs.pipe({"/a": b"abcd", "/b": b"efgh"})
    monkeypatch.setattr(fs._client, "_native_has_vector_outcomes", False)
    for name in ("preadv", "pwritev", "appendv"):
        monkeypatch.setattr(
            fs._client, name, lambda *args: pytest.fail("optional API unavailable")
        )
    with opened(fs, ["/a", "/b"], "rb") as files:
        assert files[0].read(2) == b"ab"
        assert files[1].read(2) == b"ef"
    with opened(fs, ["/a", "/b"], mode) as files:
        files[0].write(b"new-a")
        files[1].write(b"new-b")
    assert fs.cat_file("/a") == (b"abcd" if mode == "ab" else b"") + b"new-a"
    assert fs.cat_file("/b") == (b"efgh" if mode == "ab" else b"") + b"new-b"
