"""Public positions and cleanup across filtered cache and I/O cohorts."""

import errno

import fsspec
import pytest
from fsspec.core import OpenFile, OpenFiles


def _open_files(fs, paths):
    return OpenFiles(
        [OpenFile(fs, path, mode="rb") for path in paths], mode="rb", fs=fs
    )


@pytest.mark.parametrize("operation", ["cat", "cat_ranges"])
@pytest.mark.parametrize("native_index", [0, None, -1, 99, True])
def test_raised_read_errors_keep_public_positions(
    dummy_fs, monkeypatch, operation, native_index
):
    dummy_fs.batch_size = 1
    dummy_fs.auto_reconnect = False
    dummy_fs.pipe({"/first": b"a", "/later": b"b"})
    method = "read_all_many" if operation == "cat" else "read_many"
    original = getattr(dummy_fs._client, method)
    error = TimeoutError("lost read reply")
    error.transport_kind = "timeout"
    error.index = native_index
    cause = OSError("original transport cause")
    error.__cause__ = cause
    calls = []

    def read(paths, *args, **kwargs):
        calls.append(list(paths))
        if len(calls) == 2:
            raise error
        return original(paths, *args, **kwargs)

    monkeypatch.setattr(dummy_fs._client, method, read)
    with pytest.raises(TimeoutError) as raised:
        if operation == "cat":
            dummy_fs.cat(["/first", "/later"], on_error="return")
        else:
            dummy_fs.cat_ranges(["/first", "/later"], [0, 0], [1, 1])
    assert raised.value is error
    assert error.__cause__ is cause
    assert error.transport_kind == "timeout"
    assert error.index == (
        1 if type(native_index) is int and native_index == 0 else native_index
    )
    assert calls == [["/first"], ["/later"]]


@pytest.mark.parametrize("phase", ["metadata", "open", "fstat"])
def test_persistent_cache_errors_keep_public_positions(tmp_path, monkeypatch, phase):
    target = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "remote"),
        block_size=4,
        skip_instance_cache=True,
    )
    cached = fsspec.filesystem(
        "blockcache",
        fs=target,
        cache_storage=str(tmp_path / "cache"),
        cache_check=0,
        check_files=False,
        expiry_time=0,
        skip_instance_cache=True,
    )
    try:
        target.pipe(
            {path: b"abcdefgh" for path in ["/complete", "/partial-a", "/partial-b"]}
        )
        with cached.open("/complete", "rb", block_size=4) as file:
            assert file.read() == b"abcdefgh"
        if phase == "metadata":
            paths = ["/complete", "/missing"]
            expected_index = 1
            expected_error = FileNotFoundError
            failure = None
        else:
            for path, block_size in [("/partial-a", 4), ("/partial-b", 2)]:
                with cached.open(path, "rb", block_size=block_size) as file:
                    assert file.read(1) == b"a"
            paths = ["/complete", "/partial-a", "/partial-b"]
            expected_index = 2
            expected_error = TimeoutError
            failure = TimeoutError("lost cohort reply")
            failure.index = 0
            failure.transport_kind = "timeout"
            method = "open_many" if phase == "open" else "fstat_many"
            original = getattr(target._client, method)
            calls = []

            def fail_second_cohort(*args, **kwargs):
                calls.append(args)
                if len(calls) == 2:
                    raise failure
                return original(*args, **kwargs)

            monkeypatch.setattr(target._client, method, fail_second_cohort)
        with pytest.raises(expected_error) as raised:
            with _open_files(cached, paths):
                pytest.fail("failed grouped open unexpectedly succeeded")
        assert raised.value.index == expected_index
        if failure is not None:
            assert raised.value is failure
            assert failure.transport_kind == "timeout"
            assert len(calls) == 2
        assert not target._client._fds
        assert not target._client._deferred_close
    finally:
        cached.clear_cache()
        target.close()


@pytest.mark.parametrize("range_kind", ["eof", "negative", "sentinel"])
@pytest.mark.parametrize("missing", [False, True])
def test_ranges_probe_metadata_only_when_needed(
    dummy_fs, monkeypatch, range_kind, missing
):
    dummy_fs.pipe({"/bounded": b"abcdef", "/sized": b"abcdef"})
    if missing:
        dummy_fs.rm("/sized")
    original = dummy_fs._client.stat_many
    calls = []

    def stat(paths, **kwargs):
        calls.append(list(paths))
        stats, errors = original(paths, **kwargs)
        for index, path in enumerate(paths):
            if path == "/bounded":
                stats[index] = None
                errors[index] = PermissionError(errno.EACCES, "metadata denied", path)
        return stats, errors

    monkeypatch.setattr(dummy_fs._client, "stat_many", stat)
    start, end, expected = {
        "eof": (0, None, b"abcdef"),
        "negative": (-2, None, b"ef"),
        "sentinel": (2, 1, b"cdef"),
    }[range_kind]
    results = dummy_fs.cat_ranges(["/bounded", "/sized"], [0, start], [1, end])
    assert results[0] == b"a"
    if missing:
        assert isinstance(results[1], FileNotFoundError)
        assert results[1].index == 1
    else:
        assert results[1] == expected
    assert calls == [["/sized"]]


@pytest.mark.parametrize("failure_timing", ["flush", "close"])
@pytest.mark.parametrize("failed_index", [0, 1])
def test_plain_failed_group_writer_can_close_without_replay(
    dummy_fs, monkeypatch, failure_timing, failed_index
):
    dummy_fs.write_buffering = True
    files = dummy_fs.open_many(
        [OpenFile(dummy_fs, path, mode="wb") for path in ["/a", "/b"]]
    )
    failure = ConnectionError("ambiguous grouped write")
    attempts = []
    original = dummy_fs._client.pwritev

    def fail(*args, **kwargs):
        attempts.append(args)
        raise failure

    failed = files[failed_index]
    healthy = files[1 - failed_index]
    try:
        failed.write(b"bad")
        monkeypatch.setattr(dummy_fs._client, "pwritev", fail)
        if failure_timing == "flush":
            with pytest.raises(ConnectionError) as raised:
                failed.flush()
            assert raised.value is failure
        with pytest.raises(ConnectionError) as raised:
            failed.close()
        if failure_timing == "close":
            assert raised.value is failure
        else:
            assert (
                str(raised.value) == "buffered writer is unusable after a failed flush"
            )
        assert failed.closed
        assert not healthy.closed
        assert len(attempts) == 1
        monkeypatch.setattr(dummy_fs._client, "pwritev", original)
        healthy.write(b"good")
        healthy.close()
        assert all(file.closed for file in files)
        assert not dummy_fs._client._fds
        assert not dummy_fs._client._deferred_close
        assert dummy_fs.cat_file(healthy.path) == b"good"
        assert dummy_fs.cat_file(failed.path) == b""
        assert len(attempts) == 1
    finally:
        monkeypatch.setattr(dummy_fs._client, "pwritev", original)
        try:
            dummy_fs.commit_many(files)
        except ConnectionError:
            pass


def test_mixed_mode_open_rebases_metadata_errors(dummy_fs, monkeypatch):
    dummy_fs.pipe_file("/read", b"data")
    failure = PermissionError(errno.EACCES, "handle metadata denied", "/read")
    failure.index = 0

    def fail(fds):
        assert len(fds) == 1
        raise failure

    monkeypatch.setattr(dummy_fs._client, "fstat_many", fail)
    entries = [
        OpenFile(dummy_fs, "/write", mode="wb"),
        OpenFile(dummy_fs, "/read", mode="rb"),
    ]
    with pytest.raises(PermissionError) as raised:
        dummy_fs.open_many(entries)
    assert raised.value is failure
    assert failure.index == 1
    assert not dummy_fs._client._fds


def test_range_metadata_transport_error_rebases_subset(dummy_fs, monkeypatch):
    dummy_fs.pipe({"/bounded": b"a", "/eof": b"b"})
    failure = TimeoutError("lost metadata reply")
    failure.transport_kind = "timeout"
    failure.index = 0

    def fail(paths):
        assert paths == ["/eof"]
        raise failure

    monkeypatch.setattr(dummy_fs._client, "stat_many", fail)
    with pytest.raises(TimeoutError) as raised:
        dummy_fs.cat_ranges(["/bounded", "/eof"], [0, 0], [1, None])
    assert raised.value is failure
    assert failure.index == 1
    assert failure.transport_kind == "timeout"


@pytest.mark.parametrize("retry", [False, True])
@pytest.mark.parametrize("native_index", [0, None])
def test_raised_copy_errors_keep_filtered_public_positions(
    dummy_fs, monkeypatch, retry, native_index
):
    dummy_fs.auto_mkdir = True
    dummy_fs.mkdir("/directory")
    dummy_fs.pipe({"/first": b"data", "/later": b"data"})
    failure = TimeoutError("lost copy reply")
    failure.transport_kind = "timeout"
    failure.index = native_index
    calls = []

    def copy_many(pairs):
        calls.append(list(pairs))
        if retry and len(calls) == 1:
            return [4, None], {1: FileNotFoundError(errno.ENOENT, "missing parent")}
        raise failure

    monkeypatch.setattr(dummy_fs._client, "copy_many", copy_many)
    with pytest.raises(TimeoutError) as raised:
        dummy_fs.copy(
            ["/directory", "/first", "/later"],
            ["/directory-copy", "/first-copy", "/parent/later"],
            symlinks=True,
        )
    assert raised.value is failure
    assert failure.transport_kind == "timeout"
    assert failure.index == ((2 if retry else 1) if native_index == 0 else None)
    assert calls[0] == [("/first", "/first-copy"), ("/later", "/parent/later")]
    if retry:
        assert calls[1:] == [[("/later", "/parent/later")]]
    else:
        assert len(calls) == 1
