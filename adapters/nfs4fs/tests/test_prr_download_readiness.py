"""Regression coverage for streamed download setup and ownership."""

import errno

import pytest


@pytest.mark.parametrize("code", [errno.ENOENT, errno.EACCES, errno.ECONNRESET])
@pytest.mark.parametrize("paired_paths", [False, True])
def test_streamed_download_source_open_failure_preserves_destination(
    dummy_fs, tmp_path, monkeypatch, code, paired_paths
):
    dummy_fs.pipe_file("/source", b"remote payload")
    dummy_fs.max_batch_bytes = 4
    target = tmp_path / "destination"
    target.write_bytes(b"original local contents")
    native_open = dummy_fs._client.open
    source = dummy_fs._native_path("/source")
    failure = OSError(code, "injected source open failure", source)

    def fail_source_open(path, mode, **kwargs):
        if path == source and mode == "rb":
            raise failure
        return native_open(path, mode, **kwargs)

    monkeypatch.setattr(dummy_fs._client, "open", fail_source_open)
    remote = ["/source"] if paired_paths else "/source"
    local = [str(target)] if paired_paths else str(target)
    with pytest.raises(OSError) as error:
        dummy_fs.get(remote, local)
    assert error.value is failure
    assert target.read_bytes() == b"original local contents"
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("paired_paths", [False, True])
@pytest.mark.parametrize("close_error_type", [OSError, KeyboardInterrupt])
@pytest.mark.parametrize("failure_at", ["local_open", "read", "callback", "success"])
def test_streamed_download_failed_close_preserves_primary_and_defers_cleanup(
    dummy_fs, tmp_path, monkeypatch, paired_paths, close_error_type, failure_at
):
    import builtins

    from fsspec.callbacks import Callback

    payload = b"remote payload"
    dummy_fs.pipe_file("/source", payload)
    dummy_fs.max_batch_bytes = 4
    target = tmp_path / "destination"
    target.write_bytes(b"original local contents")
    primary = (
        PermissionError(errno.EACCES, "local OPEN failed", str(target))
        if failure_at == "local_open"
        else (
            RuntimeError("callback failed")
            if failure_at == "callback"
            else OSError(errno.EIO, "READ failed")
        )
    )
    cleanup = close_error_type("remote CLOSE failed")
    attempted = []
    local_open = builtins.open

    def fail_local_open(path, *args, **kwargs):
        if path == str(target):
            raise primary
        return local_open(path, *args, **kwargs)

    def fail_close(fd):
        attempted.append(fd)
        raise cleanup

    def fail_read(*args, **kwargs):
        raise primary

    class FailingCallback(Callback):
        def branch(self, path1, path2, kwargs):
            kwargs["callback"] = self

        def relative_update(self, inc=1):
            raise primary

    with monkeypatch.context() as patch:
        patch.setattr(dummy_fs._client, "close", fail_close)
        if failure_at == "local_open":
            patch.setattr(builtins, "open", fail_local_open)
        elif failure_at == "read":
            patch.setattr(dummy_fs._client, "pread", fail_read)
        kwargs = {"callback": FailingCallback()} if failure_at == "callback" else {}
        expected = cleanup if failure_at == "success" else primary
        remote = ["/source"] if paired_paths else "/source"
        local = [str(target)] if paired_paths else str(target)
        with pytest.raises(type(expected)) as error:
            dummy_fs.get(remote, local, **kwargs)
        assert error.value is expected
        if failure_at != "success":
            assert error.value.__cause__ is cleanup
        assert dummy_fs._client._fds
        assert (
            set(dummy_fs._client._fds)
            == dummy_fs._client._deferred_close
            == set(attempted)
        )
    if failure_at == "local_open":
        assert target.read_bytes() == b"original local contents"
    elif failure_at == "success":
        assert target.read_bytes() == payload
    # A later path call retries cleanup; no reader is exposed by get().
    assert dummy_fs.info("/source")["size"] == len(payload)
    assert not dummy_fs._client._fds
    assert not dummy_fs._client._deferred_close


def test_streamed_download_local_open_failure_closes_source(
    dummy_fs, tmp_path, monkeypatch
):
    import builtins

    dummy_fs.pipe_file("/source", b"remote payload")
    dummy_fs.max_batch_bytes = 4
    target = tmp_path / "destination"
    target.write_bytes(b"original local contents")
    local_open = builtins.open
    failure = PermissionError(errno.EACCES, "injected local open failure", str(target))

    def fail_local_open(path, *args, **kwargs):
        if path == str(target):
            raise failure
        return local_open(path, *args, **kwargs)

    monkeypatch.setattr(builtins, "open", fail_local_open)
    with pytest.raises(PermissionError) as error:
        dummy_fs.get("/source", str(target))
    assert error.value is failure
    assert target.read_bytes() == b"original local contents"
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("existing", [False, True])
def test_batched_get_callback_setup_failure_preserves_local_destination(
    dummy_fs, tmp_path, monkeypatch, existing
):
    from fsspec.callbacks import Callback

    fs = dummy_fs
    fs.pipe_file("/source", b"remote payload")
    target = tmp_path / "local" / "destination"
    if existing:
        target.parent.mkdir()
        target.write_bytes(b"original local contents")
    calls = []
    read_many = fs._client.read_all_many

    def record_read(paths, limit):
        calls.append(paths)
        return read_many(paths, limit)

    class FailingChild(Callback):
        def set_size(self, size):
            raise RuntimeError("child setup failed")

    class Parent(Callback):
        def branch(self, path1, path2, kwargs):
            kwargs["callback"] = FailingChild()

    monkeypatch.setattr(fs._client, "read_all_many", record_read)
    with pytest.raises(RuntimeError, match="child setup failed"):
        fs.get(["/source"], [str(target)], callback=Parent())
    assert len(calls) == 1
    if existing:
        assert target.read_bytes() == b"original local contents"
    else:
        assert not target.parent.exists()


@pytest.mark.parametrize("read_limit", [4, 16])
def test_batched_get_streams_source_that_outgrows_batch_threshold(
    dummy_fs, tmp_path, monkeypatch, read_limit
):
    fs = dummy_fs
    fs.max_batch_bytes = 4
    fs.read_all_max_total_bytes = read_limit
    fs.pipe({"/first": b"a", "/growing": b"bc", "/last": b"d"})
    original = fs._client.stat_many
    grew = False
    growing = fs._native_path("/growing")

    def stat_then_grow(paths):
        nonlocal grew
        outcome = original(paths)
        if growing in paths and not grew:
            grew = True
            fs.pipe_file("/growing", b"growing!")
        return outcome

    monkeypatch.setattr(fs._client, "stat_many", stat_then_grow)
    destinations = [tmp_path / name for name in ("first", "growing", "last")]
    fs.get(["/first", "/growing", "/last"], [str(path) for path in destinations])
    assert grew
    assert [path.read_bytes() for path in destinations] == [b"a", b"growing!", b"d"]


def test_batched_get_drops_previous_payload_before_next_native_read(
    dummy_fs, tmp_path, monkeypatch
):
    import weakref

    class Payload(bytearray):
        pass

    fs = dummy_fs
    fs.batch_size = 1
    fs.pipe({"/first": b"a", "/second": b"b"})
    original = fs._client.read_all_many
    payloads = []

    def read_batch(paths, limit):
        assert all(ref() is None for ref in payloads)
        data, errors = original(paths, limit)
        data = [Payload(value) for value in data]
        payloads.extend(weakref.ref(value) for value in data)
        return data, errors

    monkeypatch.setattr(fs._client, "read_all_many", read_batch)
    destinations = [tmp_path / name for name in ("first", "second")]
    fs.get(["/first", "/second"], [str(path) for path in destinations])
    assert len(payloads) == 2
    assert [path.read_bytes() for path in destinations] == [b"a", b"b"]


@pytest.mark.parametrize(
    "native_error", [NotImplementedError("read unavailable"), 0xFFFF_FFFE]
)
def test_batched_get_preserves_unsupported_failure(
    dummy_fs, tmp_path, monkeypatch, native_error
):
    fs = dummy_fs
    fs.pipe_file("/source", b"remote payload")
    target = tmp_path / "destination"
    target.write_bytes(b"original local contents")
    monkeypatch.setattr(
        fs._client, "read_all_many", lambda *_: ([None], {0: native_error})
    )

    with pytest.raises(NotImplementedError) as error:
        fs.get(["/source"], [str(target)])
    assert error.value.index == 0
    assert target.read_bytes() == b"original local contents"


@pytest.mark.parametrize("paired", [False, True])
@pytest.mark.parametrize("existing", [False, True])
@pytest.mark.parametrize("data", [b"", b"remote"])
def test_zero_read_budget_checks_source_before_opening_destination(
    dummy_fs, tmp_path, paired, existing, data
):
    fs = dummy_fs
    fs.pipe_file("/source", data)
    fs.read_all_max_total_bytes = 0
    target = tmp_path / "destination"
    if existing:
        target.write_bytes(b"original")

    def download():
        if paired:
            fs.get(["/source"], [str(target)])
        else:
            fs.get_file("/source", str(target))

    if data:
        with pytest.raises(OSError) as caught:
            download()
        assert caught.value.errno == errno.EFBIG
        if existing:
            assert target.read_bytes() == b"original"
        else:
            assert not target.exists()
    else:
        download()
        assert target.read_bytes() == b""
    assert not fs._client._fds


@pytest.mark.parametrize("data", [b"abc", b"abcdefgh"])
def test_get_with_omitted_size_uses_bounded_reads(
    dummy_fs, tmp_path, monkeypatch, data
):
    fs = dummy_fs
    fs.pipe_file("/source", data)
    fs.read_all_max_total_bytes = 4
    original = fs._client.stat_many

    def stat_without_size(paths):
        attrs, errors = original(paths)
        return [
            (
                {key: value for key, value in info.items() if key != "size"}
                if info
                else info
            )
            for info in attrs
        ], errors

    monkeypatch.setattr(fs._client, "stat_many", stat_without_size)
    target = tmp_path / "destination"
    fs.get(["/source"], [str(target)])
    assert target.read_bytes() == data
    assert not fs._client._fds
