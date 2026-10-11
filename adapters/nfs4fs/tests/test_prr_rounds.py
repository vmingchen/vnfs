"""Regressions from successive production readiness reviews."""

import pytest


@pytest.mark.parametrize("failure", [OSError, KeyboardInterrupt])
@pytest.mark.parametrize("second_failure", [False, True])
def test_transaction_group_close_failure_still_closes_all_siblings(
    fs, monkeypatch, failure, second_failure
):
    from fsspec.core import OpenFile

    fs.pipe_file("/reader", b"original")
    error = failure("first close failure")
    attempted = []
    with pytest.raises(failure) as caught:
        with fs.transaction:
            files = fs.open_many(
                [
                    OpenFile(fs, "/writer", "wb"),
                    OpenFile(fs, "/reader", "rb"),
                    OpenFile(fs, "/reader", "rb"),
                ]
            )
            files[0].write(b"pending")
            for index, file in enumerate(files):
                close = file.close

                def record_close(index=index, close=close):
                    attempted.append(index)
                    close()
                    if index == 0:
                        raise error
                    if index == 1 and second_failure:
                        raise OSError("second close failure")

                monkeypatch.setattr(file, "close", record_close)
            fs.commit_many(files)
    try:
        assert caught.value is error
        assert attempted == [0, 1, 2]
        assert all(file.closed for file in files)
        assert not fs._client._fds
        assert not fs.exists("/writer")
    finally:
        # Keep a pre-fix failure from leaking its readers into other tests.
        for file in files:
            if not file.closed:
                try:
                    file.close()
                except OSError:
                    pass


@pytest.fixture(params=["dummy_fs", "nfs_fs"])
def fs(request):
    return request.getfixturevalue(request.param)


@pytest.mark.parametrize("name", ["a" * 255, "é" * 127], ids=["ascii", "utf8"])
@pytest.mark.parametrize("mode", ["wb", "ab", "xb"])
def test_transaction_accepts_long_destination_names(fs, name, mode):
    path = "/" + name
    # Establish that the backend accepts the destination name itself.
    fs.pipe_file(path, b"original")
    if mode == "xb":
        fs.rm_file(path)
    with fs.transaction:
        with fs.open(path, mode) as file:
            file.write(b"new")
    assert fs.cat_file(path) == (b"originalnew" if mode == "ab" else b"new")
    assert fs.ls("/", detail=False) == ["nfs4://" + path]
    assert not fs._client._fds


@pytest.mark.parametrize("mode", ["ab", "ab+"])
@pytest.mark.parametrize("buffered", [False, True])
@pytest.mark.parametrize("known_sizes", [False, True])
def test_grouped_append_starts_at_eof(fs, monkeypatch, mode, buffered, known_sizes):
    from fsspec.core import OpenFile

    paths = ["/append-a", "/append-b"]
    values = [b"original", b"longer-original"]
    fs.pipe(dict(zip(paths, values)))
    stat_many = fs._client.fstat_many
    calls = []

    def record(fds):
        calls.append(len(fds))
        return stat_many(fds)

    monkeypatch.setattr(fs._client, "fstat_many", record)
    files = fs.open_many(
        [OpenFile(fs, path, mode) for path in paths],
        write_buffering=buffered,
        sizes=[1, 1] if known_sizes else None,
    )
    try:
        # A caller-supplied read-size hint must not become an append position.
        assert [file.tell() for file in files] == list(map(len, values))
        for file in files:
            file.truncate()
            file.write(b"+")
    finally:
        fs.commit_many(files)
    assert [fs.cat_file(path) for path in paths] == [v + b"+" for v in values]
    assert calls == [2]
    assert not fs._client._fds


@pytest.mark.parametrize("cache_type", ["none", "readahead"])
def test_grouped_read_rejects_directory_and_closes_siblings(fs, cache_type):
    from fsspec.core import OpenFile

    fs.pipe_file("/file", b"original")
    fs.mkdir("/directory")
    opened = []
    try:
        with pytest.raises(IsADirectoryError):
            opened = fs.open_many(
                [OpenFile(fs, p, "rb") for p in ["/file", "/directory"]],
                cache_type=cache_type,
            )
    finally:
        fs.commit_many(opened)
    assert not fs._client._fds
    assert fs.cat_file("/file") == b"original"


@pytest.mark.parametrize("write_fails", [False, True])
def test_exclusive_transfer_retains_failed_close_for_cleanup(
    fs, monkeypatch, write_fails
):
    import errno

    failed_fds = []

    def fail_close(fd):
        failed_fds.append(fd)
        raise OSError(errno.EIO, "injected close failure")

    def fail_write(*args):
        raise OSError(errno.ENOSPC, "injected write failure")

    with monkeypatch.context() as patch:
        patch.setattr(fs._client, "close", fail_close)
        if write_fails:
            patch.setattr(fs._client, "pwrite", fail_write)
        with pytest.raises(OSError) as error:
            fs.pipe_file("/exclusive", b"payload", mode="create")
        assert error.value.errno == (errno.ENOSPC if write_fails else errno.EIO)
    assert len(failed_fds) == 1
    # Every still-open token must have an owner that will retry cleanup.
    assert set(fs._client._fds) <= fs._client._deferred_close
    assert fs.exists("/exclusive") == (not write_fails)
    assert not fs._client._fds
    if not write_fails:
        assert fs.cat_file("/exclusive") == b"payload"


@pytest.mark.parametrize("failure", ["branch", "enter", "set_size", "source_open"])
def test_streamed_upload_setup_failure_preserves_destination(
    fs, tmp_path, monkeypatch, failure
):
    import builtins

    from fsspec.callbacks import Callback

    fs.max_batch_bytes = 4
    source = tmp_path / "upload-source"
    source.write_bytes(b"more than four bytes")
    fs.pipe_file("/destination", b"original")

    class FailingCallback(Callback):
        def branched(self, *args, **kwargs):
            if failure == "branch":
                raise RuntimeError("injected setup failure")
            return Child()

    class Child(Callback):
        def __enter__(self):
            if failure == "enter":
                raise RuntimeError("injected setup failure")
            return super().__enter__()

        def set_size(self, size):
            if failure == "set_size":
                raise RuntimeError("injected setup failure")
            return super().set_size(size)

    original_open = builtins.open

    def fail_source_open(path, mode="r", *args, **kwargs):
        if str(path) == str(source) and mode == "rb":
            raise RuntimeError("injected setup failure")
        return original_open(path, mode, *args, **kwargs)

    if failure == "source_open":
        monkeypatch.setattr(builtins, "open", fail_source_open)
    with pytest.raises(RuntimeError, match="injected setup failure"):
        fs.put(str(source), "/destination", callback=FailingCallback())
    assert not fs._client._fds
    assert fs.cat_file("/destination") == b"original"
