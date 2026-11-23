"""Reproductions for production-readiness review findings."""

import errno
import threading

import pytest
from fsspec.core import OpenFile
from nfs4fs import Nfs4FileSystem


@pytest.fixture(params=["dummy_fs", "nfs_fs"])
def fs(request):
    return request.getfixturevalue(request.param)


def _pooled_filesystem(backend, tmp_path, nfs_parameters=None, size=2):
    if backend == "dummy":
        return Nfs4FileSystem(
            backend="dummy",
            dummy_root=str(tmp_path / "pooled"),
            connection_pool_size=size,
            skip_instance_cache=True,
        )
    host, minor_version = nfs_parameters
    return Nfs4FileSystem(
        host=host,
        root=f"git/nfs4fs_pool_{threading.get_ident()}_{tmp_path.name}",
        minor_version=minor_version,
        auth="auth_sys",
        connection_pool_size=size,
        skip_instance_cache=True,
    )


def test_failed_mixed_mmap_setup_closes_descriptors_by_owner(tmp_path, monkeypatch):
    with _pooled_filesystem("dummy", tmp_path) as fs:
        fs.pipe({"/reader": b"payload", "/writer": b"original"})
        client = fs._client
        fstat_many = client.fstat_many
        close_many = client.close_many
        fstat_calls = 0
        closed_owners = []

        def fail_size_refresh(tokens):
            nonlocal fstat_calls
            fstat_calls += 1
            if fstat_calls == 2:
                assert len({client._resolve(token)[0] for token in client._fds}) == 2
                raise OSError(errno.EIO, "mmap size refresh failed")
            return fstat_many(tokens)

        def check_close(tokens):
            owners = {client._resolve(token)[0] for token in tokens}
            assert len(owners) == 1
            closed_owners.extend(owners)
            return close_many(tokens)

        monkeypatch.setattr(client, "fstat_many", fail_size_refresh)
        monkeypatch.setattr(client, "close_many", check_close)
        with pytest.raises(OSError, match="mmap size refresh failed") as raised:
            fs.open_many(
                [
                    OpenFile(fs, "/reader", mode="rb"),
                    OpenFile(fs, "/writer", mode="wb"),
                ],
                cache_type="mmap",
            )
        assert raised.value.errno == errno.EIO
        assert raised.value.__cause__ is None
        assert len(closed_owners) == 2
        assert set(closed_owners) == {0, 1}
        assert not client._fds
        assert not client._deferred_close


@pytest.fixture
def nfs_parameters():
    from ._servers import nfs_config, nfs_reachable

    host, minor_version = nfs_config()
    if not nfs_reachable(host, minor_version):
        pytest.skip(f"NFS server {host!r} is not reachable")
    return host, minor_version


@pytest.mark.parametrize("mode", ["rb+", "wb+", "ab+", "xb+"])
@pytest.mark.parametrize("auto_reconnect", [False, True])
def test_grouped_read_recovery_never_replays_writable_open(
    fs, monkeypatch, mode, auto_reconnect
):
    fs.auto_reconnect = auto_reconnect
    fs.pipe_file("/reader", b"reader")
    if mode == "rb+":
        fs.pipe_file("/update", b"KEEP-ME")

    entries = [OpenFile(fs, "/reader", mode="rb"), OpenFile(fs, "/update", mode=mode)]
    files = fs.open_many(entries)
    reader, writer = files
    writer.write(b"KEEP-ME")
    writer.seek(0)

    original_open_many = fs._client.open_many
    recovery_opens = []

    def record_open_many(paths, modes):
        recovery_opens.append(list(modes))
        return original_open_many(paths, modes)

    monkeypatch.setattr(fs._client, "open_many", record_open_many)
    original_pread_many = fs._client.preadv
    attempts = 0

    def fail_once(fds, offsets, lengths):
        nonlocal attempts
        attempts += 1
        if attempts == 1:
            raise ConnectionError("injected grouped read transport failure")
        return original_pread_many(fds, offsets, lengths)

    monkeypatch.setattr(fs._client, "preadv", fail_once)
    if auto_reconnect:
        assert reader.read(1) == b"r"
        assert recovery_opens
        assert recovery_opens == [["rb"]]
    else:
        with pytest.raises(ConnectionError, match="grouped read transport failure"):
            reader.read(1)
        assert not recovery_opens
    assert fs.info("/update")["size"] == len(b"KEEP-ME")
    fs.commit_many(files)
    assert not fs._client._fds


def test_reconnected_scalar_reader_does_not_switch_to_replacement_file(fs):
    fs.pipe_file("/data", b"abcdef")
    reader = fs.open("/data", "rb", cache_type="none")
    try:
        assert reader.read(2) == b"ab"
        fs.mv("/data", "/old")
        fs.pipe_file("/data", b"WXYZUV")

        client = fs._client._clients[0]
        old_native = client._native
        calls = 0

        class FailOnceProxy:
            def __getattr__(self, name):
                target = getattr(old_native, name)
                if name != "stat_many":
                    return target

                def fail_once(path):
                    nonlocal calls
                    calls += 1
                    if calls == 1:
                        raise ConnectionError("injected metadata transport failure")
                    return target(path)

                return fail_once

        client._native = FailOnceProxy()
        fs.info("/data")

        with pytest.raises(OSError) as error:
            reader.read(2)
        assert error.value.errno == errno.ESTALE
    finally:
        reader.close()


@pytest.mark.parametrize("cache_type", ["none", "readahead"])
def test_scalar_reader_whole_file_retry_stays_pinned(fs, cache_type):
    fs.pipe_file("/data", b"original")
    with fs.open("/data", "rb", cache_type=cache_type) as reader:
        fs._client.reconnect()
        fs.mv("/data", "/old")
        fs.pipe_file("/data", b"replacement")

        with pytest.raises(OSError) as error:
            reader.read()
        assert error.value.errno == errno.ESTALE
        assert reader.tell() == 0

        with pytest.raises(OSError) as error:
            reader.read()
        assert error.value.errno == errno.ESTALE
        assert reader.tell() == 0

        fs.rm("/data")
        fs.mv("/old", "/data")
        assert reader.read() == b"original"


def test_closed_scalar_reader_size_does_not_open_descriptor(fs):
    fs.pipe_file("/data", b"before")
    reader = fs.open("/data", "rb", cache_type="none")
    reader.close()
    fs.pipe_file("/data", b"new size")

    assert reader.size == len(b"new size")
    assert not fs._client._fds


def test_scalar_reader_rechecks_identity_after_reopen_fstat_fails(fs, monkeypatch):
    fs.pipe_file("/data", b"original-payload")
    reader = fs.open("/data", "rb", cache_type="none")
    try:
        assert reader.read(2) == b"or"
        fs._client.reconnect()
        fs.mv("/data", "/old")
        fs.pipe_file("/data", b"replacement-data")

        original_fstat = fs._client.fstat
        calls = 0

        def fail_once(fd):
            nonlocal calls
            calls += 1
            if calls == 1:
                raise ConnectionError("injected reopened descriptor fstat failure")
            return original_fstat(fd)

        monkeypatch.setattr(fs._client, "fstat", fail_once)
        with pytest.raises(ConnectionError, match="injected reopened descriptor"):
            reader.read(2)
        with pytest.raises(OSError) as error:
            reader.read(2)
        assert error.value.errno == errno.ESTALE
        assert calls == 2
    finally:
        reader.close()


def _replace_path_after_reconnect_trigger(fs):
    fs.mv("/data", "/old")
    fs.pipe_file("/data", b"WXYZUV")
    client = fs._client._clients[0]
    old_native = client._native

    class FailOnceProxy:
        def __getattr__(self, name):
            target = getattr(old_native, name)
            if name != "stat_many":
                return target

            def fail_once(paths):
                raise ConnectionError("injected metadata transport failure")

            return fail_once

    client._native = FailOnceProxy()
    fs.info("/data")


@pytest.mark.parametrize("trigger", ["retired", "read-error", "deferred"])
@pytest.mark.parametrize("batch_size", [1, 8])
def test_group_recovery_keeps_healthy_reader_when_sibling_was_replaced(
    fs, monkeypatch, trigger, batch_size
):
    fs.batch_size = batch_size
    fs.pipe({"/healthy": b"healthy", "/replaced": b"original"})
    files = fs.open_many(
        [OpenFile(fs, path, mode="rb") for path in ["/healthy", "/replaced"]],
        block_size=2,
    )
    try:
        fs.mv("/replaced", "/old")
        fs.pipe_file("/replaced", b"replacement")
        if trigger == "retired":
            fs._client.reconnect()
        elif trigger == "deferred":
            ensure_ready = fs._client.ensure_ready
            recovered = False

            def reconnect_when_checked():
                nonlocal recovered
                if not recovered:
                    recovered = True
                    fs._client.reconnect()
                ensure_ready()

            monkeypatch.setattr(fs._client, "ensure_ready", reconnect_when_checked)
        else:
            preadv = fs._client.preadv
            attempts = 0

            def fail_once(*args):
                nonlocal attempts
                attempts += 1
                if attempts == 1:
                    raise ConnectionError("injected vector read failure")
                return preadv(*args)

            monkeypatch.setattr(fs._client, "preadv", fail_once)

        assert files[0].read(2) == b"he"
        with pytest.raises(OSError) as error:
            files[1].read(2)
        assert error.value.errno == errno.ESTALE
        assert error.value.filename == "/replaced"
        assert files[0].read() == b"althy"
    finally:
        fs.commit_many(files)
    assert not fs._client._fds


@pytest.mark.parametrize("phase", ["open", "fstat"])
@pytest.mark.parametrize("kind", ["auth", "connection"])
def test_grouped_restore_transport_failure_does_not_block_live_reader(
    fs, monkeypatch, phase, kind
):
    fs.pipe({"/live": b"healthy-file", "/stale": b"stale-file"})
    files = fs.open_many(
        [OpenFile(fs, path, mode="rb") for path in ["/live", "/stale"]],
        block_size=2,
    )
    live, stale = files
    try:
        fs._client.reconnect()
        fs.batch_size = 1
        assert live.read(1) == b"h"
        pinned = live._raw._fd
        assert fs._client.descriptor_valid(pinned)
        assert not fs._client.descriptor_valid(stale._raw._fd)
        fs.batch_size = 8
        setup_calls = []

        def fail_restore(*args):
            setup_calls.append(args)
            error = OSError(errno.EIO, "stale reader could not be restored")
            error.transport_kind = kind
            raise error

        monkeypatch.setattr(
            fs._client, "open_many" if phase == "open" else "fstat_many", fail_restore
        )
        live.seek(5)
        assert live.read(1) == b"h"
        assert setup_calls, "the speculative reader must reach the failure"
        assert live._raw._fd == pinned
        assert fs._client.descriptor_valid(pinned)
        with pytest.raises(
            OSError, match="stale reader could not be restored"
        ) as failed:
            stale.read(1)
        assert failed.value.filename == "/stale"
        assert failed.value.index == 1
        assert failed.value.transport_kind == kind
        assert failed.value.failure_state == "not_attempted"
        assert set(fs._client._fds) == {pinned}
    finally:
        fs.commit_many(files)
    assert not fs._client._fds


@pytest.mark.parametrize("phase", ["open", "fstat"])
def test_attributed_reader_restore_failure_isolated(fs, monkeypatch, phase):
    fs.pipe({"/live": b"healthy", "/stale": b"unavailable"})
    files = fs.open_many(
        [OpenFile(fs, path, mode="rb") for path in ["/live", "/stale"]],
        block_size=2,
    )
    live, stale = files
    try:
        fs._client.reconnect()
        method = "open_many" if phase == "open" else "fstat_many"
        calls = []

        def fail_restore(values, *args):
            calls.append(values)
            error = OSError(errno.EIO, "attributed restore failed")
            error.transport_kind = "auth"
            error.index = len(values) - 1
            raise error

        monkeypatch.setattr(fs._client, method, fail_restore)
        assert live.read(1) == b"h"
        assert len(calls[0]) == 2
        assert fs._client.descriptor_valid(live._raw._fd)
        live.seek(4)
        assert live.read(1) == b"t"
        with pytest.raises(OSError, match="attributed restore failed") as failed:
            stale.read(1)
        assert failed.value.filename == "/stale"
        assert failed.value.index == 1
        assert failed.value.transport_kind == "auth"
        assert failed.value.failure_state == "not_attempted"
    finally:
        fs.commit_many(files)
    assert not fs._client._fds


@pytest.mark.parametrize("failed_first", [False, True])
def test_grouped_read_retries_healthy_owner_after_another_owner_fails(
    tmp_path, monkeypatch, failed_first
):
    fs = _pooled_filesystem("dummy", tmp_path)
    fs.pipe({"/live": b"abc", "/stale": b"def"})
    files = fs.open_many(
        [OpenFile(fs, path, mode="rb") for path in ["/live", "/stale"]],
        block_size=2,
    )
    live, stale = files
    try:
        client = fs._client
        owner, _, _ = client._resolve(live._raw._fd)
        client.close(stale._raw._fd)
        client._next_client = 1 - owner
        stale_fd = stale._raw._ensure_open()
        assert client._resolve(stale_fd)[0] != owner
        preadv = client.preadv
        reconnect = client.reconnect_descriptor
        failures = {}
        for file, prefix in [(live, b"a"), (stale, b"d")]:
            error = ConnectionError("READ reply unavailable")
            error.read_prefix = prefix
            error.read_offset = 0
            error.failure_state = "failed"
            failures[file._raw._fd] = error

        offsets_seen = []

        def read(tokens, offsets, lengths):
            assert len(tokens) == 1
            offsets_seen.extend(offsets)
            if offsets == [0]:
                return [failures[tokens[0]]]
            return preadv(tokens, offsets, lengths)

        def revive(fd):
            if fd == stale_fd:
                raise ConnectionError("owner unavailable")
            reconnect(fd)

        monkeypatch.setattr(client, "preadv", read)
        monkeypatch.setattr(client, "reconnect_descriptor", revive)
        order = [stale, live] if failed_first else files
        data, errors = live._buffer_group._pread_many(order, [0, 0], [3, 3])
        live_index = 1 if failed_first else 0
        assert data[live_index] == b"abc"
        assert errors == {1 - live_index: failures[stale_fd]}
        assert errors[1 - live_index].read_prefix == b"d"
        assert offsets_seen == [0, 0, 1]
    finally:
        fs.commit_many(files)
        fs.close()


def test_legacy_group_retries_transient_restore_failure(dummy_fs, monkeypatch):
    fs = dummy_fs
    fs.pipe({"/first": b"abc", "/second": b"def"})
    files = fs.open_many(
        [OpenFile(fs, path, mode="rb") for path in ["/first", "/second"]],
        block_size=2,
    )
    try:
        client = fs._client
        client.reconnect()
        original = client.open_many
        calls = []

        def transient(paths, modes):
            calls.append(paths)
            if len(calls) == 1:
                raise ConnectionError("transient restore rejection")
            return original(paths, modes)

        monkeypatch.setattr(client, "_native_has_vector_outcomes", False)
        monkeypatch.setattr(client, "open_many", transient)
        assert files[0].read(1) == b"a"
        assert calls[0] == ["/first", "/second"]
        assert files[1].read(1) == b"d"
    finally:
        fs.commit_many(files)
    assert not fs._client._fds


def test_uncached_read_error_keeps_cursor_at_call_start(fs, monkeypatch):
    fs.pipe_file("/source", b"abcdef")
    with fs.open("/source", "rb", cache_type="none") as reader:
        reader.seek(1)
        pread = fs._client.pread
        offsets = []

        def fail_second(fd, length, offset):
            offsets.append(offset)
            if len(offsets) == 1:
                length = 2
            elif len(offsets) == 2:
                raise OSError(errno.EIO, "injected read failure")
            return pread(fd, length, offset)

        monkeypatch.setattr(fs._client, "pread", fail_second)
        with pytest.raises(OSError, match="injected read failure"):
            reader.read(4)
        assert offsets == [1, 3]
        assert reader.tell() == 1
        assert reader.read(4) == b"bcde"
        assert reader.tell() == 5


def test_reconnected_group_reader_rejects_replacement_file(fs):
    fs.pipe_file("/data", b"abcdef")
    entries = [OpenFile(fs, "/data", mode="rb")]
    files = fs.open_many(entries, block_size=2)
    try:
        assert files[0].read(2) == b"ab"
        _replace_path_after_reconnect_trigger(fs)
        assert files[0].read(2) == b"cd"  # already cached from the original file
        with pytest.raises(OSError) as error:
            files[0].read(2)
        assert error.value.errno == errno.ESTALE
    finally:
        fs.commit_many(files)


def test_pipelined_reader_rejects_replacement_file(fs):
    fs.pipe_file("/data", b"abcdefgh")
    chunks = []

    def receive(offset, data):
        chunks.append((offset, data))
        if offset == 0:
            _replace_path_after_reconnect_trigger(fs)

    with pytest.raises(OSError) as error:
        fs.read_stream_pipelined(
            "/data",
            receive,
            workers=1,
            chunk_size=4,
            max_in_flight=1,
            max_buffered_bytes=4,
        )
    assert error.value.errno == errno.ESTALE
    assert chunks == [(0, b"abcd")]


def test_pipelined_reader_tracks_identity_per_worker(tmp_path, monkeypatch):
    fs = _pooled_filesystem("dummy", tmp_path, size=2)
    try:
        fs.pipe_file("/data", b"AAAABBBB")
        original_open = fs._client.open_many_independent
        opened = []

        def open_across_replacement(paths, modes):
            first = original_open(paths[:1], modes[:1])
            fs.mv("/data", "/version-a")
            fs.pipe_file("/data", b"CCCCDDDD")
            second = original_open(paths[1:], modes[1:])
            opened.extend(first + second)
            return first + second

        monkeypatch.setattr(
            fs._client, "open_many_independent", open_across_replacement
        )
        chunks = []

        def receive(offset, data):
            chunks.append((offset, data))
            if offset == 0:
                fs.mv("/data", "/version-b")
                fs.mv("/version-a", "/data")
                fs._client.reconnect_descriptor(opened[1])

        with pytest.raises(OSError) as error:
            fs.read_stream_pipelined(
                "/data",
                receive,
                workers=2,
                chunk_size=4,
                max_in_flight=1,
                max_buffered_bytes=4,
            )
        assert error.value.errno == errno.ESTALE
        assert chunks == [(0, b"AAAA")]
    finally:
        fs.close()


@pytest.mark.parametrize("backend", ["dummy", "nfs"])
@pytest.mark.parametrize("pool_size", [2, 3])
def test_group_recovery_keeps_restored_descriptors_on_one_pool_owner(
    request, tmp_path, backend, pool_size
):
    parameters = request.getfixturevalue("nfs_parameters") if backend == "nfs" else None
    fs = _pooled_filesystem(backend, tmp_path, parameters, size=pool_size)
    try:
        if backend == "nfs":
            fs.mkdir("/", create_parents=True)
        fs.pipe_file("/a", b"alpha")
        fs.pipe_file("/b", b"bravo")
        entries = [OpenFile(fs, "/a", mode="rb"), OpenFile(fs, "/b", mode="rb")]
        files = fs.open_many(entries)
        owner = fs._client._fds[files[0]._fd][0]
        fs._client._reconnect_client(owner)
        assert files[0].read(1) == b"a"
        assert files[1].read(1) == b"b"
        owner = fs._client._fds[files[0]._fd][0]
        fs._client._reconnect_client(owner)
        for file in files:
            file._raw._ensure_open()
        owners = {fs._client._fds[file._fd][0] for file in files}
        assert len(owners) > 1
        fs.commit_many(files)
        assert not fs._client._fds
    finally:
        try:
            fs.rm("/", recursive=True)
        except Exception:
            pass
        fs.close()


def test_group_read_recovery_keeps_healthy_readers_vectorized(fs, monkeypatch):
    fs.pipe({"/a": b"alpha", "/b": b"bravo"})
    files = fs.open_many([OpenFile(fs, "/a", mode="rb"), OpenFile(fs, "/b", mode="rb")])
    original_open_many = fs._client.open_many
    open_batches = []

    def record_open_many(paths, modes):
        open_batches.append((list(paths), list(modes)))
        return original_open_many(paths, modes)

    monkeypatch.setattr(fs._client, "open_many", record_open_many)
    original_pread_many = fs._client.preadv
    attempts = 0
    vector_sizes = []

    def fail_once(fds, offsets, lengths):
        nonlocal attempts
        attempts += 1
        vector_sizes.append(len(fds))
        if attempts == 1:
            raise ConnectionError("injected grouped reader failure")
        return original_pread_many(fds, offsets, lengths)

    monkeypatch.setattr(fs._client, "preadv", fail_once)
    try:
        assert files[0].read(1) == b"a"
        assert open_batches == [
            ([fs._native_path("/a"), fs._native_path("/b")], ["rb", "rb"])
        ]
        assert vector_sizes == [2, 2]
    finally:
        fs.commit_many(files)
    assert not fs._client._fds


def test_public_nfs_opens_of_same_path_keep_independent_state(nfs_fs):
    fs = nfs_fs
    fs.pipe_file("/same", b"abcdef")
    first = fs.open("/same", "rb", cache_type="none")
    second = fs.open("/same", "rb", cache_type="none")
    try:
        assert first.read(2) == b"ab"
        assert second.read(2) == b"ab"
        assert first.read(2) == b"cd"
        first.close()
        assert second.read(2) == b"cd"
    finally:
        first.close()
        second.close()


def test_grouped_nfs_open_state_survives_path_readback(nfs_fs):
    fs = nfs_fs
    entries = [OpenFile(fs, "/first", mode="wb"), OpenFile(fs, "/second", mode="wb")]
    files = fs.open_many(entries)
    try:
        files[0].write(b"before")
        assert fs.cat_file("/first") == b"before"
        files[1].write(b"after")
    finally:
        fs.commit_many(files)
    assert fs.cat_file("/second") == b"after"


def test_single_writer_nfs_append_preserves_complete_records(nfs_fs):
    fs = nfs_fs
    records = [f"record-{index:04d}\n".encode() for index in range(100)]
    with fs.open("/append", "ab", cache_type="none") as file:
        for record in records:
            assert file.write(record) == len(record)
    assert fs.cat_file("/append") == b"".join(records)


def test_append_native_contract_does_not_promise_atomic_offset(dummy_fs):
    method = dummy_fs._client._clients[0]._native_module.NfsClient.append_many
    documentation = (method.__doc__ or "").lower()
    assert "not atomic across clients" in documentation
    assert "one writer per" in documentation and "file or coordinate" in documentation


def test_cat_enforces_aggregate_budget_when_files_grow_after_stat(fs, monkeypatch):
    fs.batch_size = 1
    fs.max_batch_bytes = 4
    fs.read_all_max_total_bytes = 8
    fs.pipe({"/a": b"aaaa", "/b": b"bbbb"})
    stat_many = fs._client.stat_many
    grew = False

    def stat_then_grow(paths):
        nonlocal grew
        result = stat_many(paths)
        if not grew:
            grew = True
            fs.pipe({"/a": b"a" * 8, "/b": b"b" * 8})
        return result

    monkeypatch.setattr(fs._client, "stat_many", stat_then_grow)
    with pytest.raises(OSError) as error:
        fs.cat(["/a", "/b"])
    assert error.value.errno == errno.EFBIG


def test_streamed_cat_fallback_enforces_budget_as_file_grows(fs, monkeypatch):
    fs.batch_size = 1
    fs.max_batch_bytes = 2
    fs.read_all_max_total_bytes = 8
    fs.transfer_chunk_size = 4
    fs.pipe_file("/large", b"abcd")
    stat_many = fs._client.stat_many
    grew = False

    def stat_then_grow(paths):
        nonlocal grew
        result = stat_many(paths)
        if not grew:
            grew = True
            fs.pipe_file("/large", b"x" * 16)
        return result

    monkeypatch.setattr(fs._client, "stat_many", stat_then_grow)
    with pytest.raises(OSError) as error:
        fs.cat(["/large"])
    assert error.value.errno == errno.EFBIG


def test_scalar_reader_recovers_same_file_after_reconnect(fs):
    fs.pipe_file("/data", b"abcdef")
    reader = fs.open("/data", "rb", cache_type="none")
    try:
        assert reader.read(2) == b"ab"
        fs._client.reconnect()
        assert reader.read(2) == b"cd"
    finally:
        reader.close()


def test_pipelined_reader_fails_closed_if_initial_identity_capture_fails(
    fs, monkeypatch
):
    fs.pipe_file("/data", b"original-payload")
    original_reconnect = fs._client.reconnect_descriptor

    def fail_initial_fstat(fd):
        raise ConnectionError("injected initial fstat failure")

    def replace_after_reconnect(fd):
        result = original_reconnect(fd)
        fs.mv("/data", "/old")
        fs.pipe_file("/data", b"replacement-data")
        return result

    monkeypatch.setattr(fs._client, "fstat", fail_initial_fstat)
    monkeypatch.setattr(fs._client, "reconnect_descriptor", replace_after_reconnect)
    chunks = []
    with pytest.raises(OSError) as error:
        fs.read_stream_pipelined(
            "/data",
            lambda offset, data: chunks.append((offset, data)),
            workers=1,
            chunk_size=8,
            max_in_flight=1,
            max_buffered_bytes=8,
        )
    assert error.value.errno == errno.ESTALE
    assert chunks == []


def test_cat_preserves_remaining_budget_when_growth_switches_to_streaming(
    fs, monkeypatch
):
    fs.batch_size = 1
    fs.max_batch_bytes = 2
    fs.read_all_max_total_bytes = 8
    fs.pipe({"/a": b"aa", "/b": b"bb"})
    stat_many = fs._client.stat_many
    read_all_many = fs._client.read_all_many
    grew = False
    vector_budgets = []
    stream_budgets = []
    read_streamed = fs._read_one_streamed

    def stat_then_grow(paths):
        nonlocal grew
        stats = stat_many(paths)
        if not grew:
            grew = True
            fs.pipe_file("/b", b"x" * 8)
        return stats

    def read_with_budget(paths, max_total_bytes=None):
        vector_budgets.append(max_total_bytes)
        if max_total_bytes is None:
            return read_all_many(paths)
        return read_all_many(paths, max_total_bytes)

    def stream_with_budget(path, **kwargs):
        stream_budgets.append(kwargs["max_bytes"])
        return read_streamed(path, **kwargs)

    monkeypatch.setattr(fs, "_read_one_streamed", stream_with_budget)
    monkeypatch.setattr(fs._client, "stat_many", stat_then_grow)
    monkeypatch.setattr(fs._client, "read_all_many", read_with_budget)
    with pytest.raises(OSError) as error:
        fs.cat(["/a", "/b"])
    assert error.value.errno == errno.EFBIG
    assert vector_budgets == [2, 2]
    assert stream_budgets == [6]


@pytest.mark.parametrize("mutation", ["append", "chmod"])
@pytest.mark.parametrize("reader_kind", ["scalar", "group", "pipeline"])
def test_recovery_identity_survives_in_place_mutation(fs, mutation, reader_kind):
    fs.pipe_file("/data", b"abcdefgh")

    def mutate_and_reconnect():
        before = fs.info("/data")
        if mutation == "append":
            with fs.open("/data", "ab") as writer:
                writer.write(b"tail")
        else:
            fs._client.chmod(fs._native_path("/data"), 0o600)
        after = fs.info("/data")
        assert before["fileid"] == after["fileid"]
        assert before["created_ns"] != after["created_ns"]
        fs._client.reconnect()

    if reader_kind == "pipeline":
        chunks = []

        def receive(offset, data):
            chunks.append(data)
            if offset == 0:
                mutate_and_reconnect()

        assert (
            fs.read_stream_pipelined(
                "/data",
                receive,
                workers=1,
                chunk_size=2,
                max_in_flight=1,
                max_buffered_bytes=2,
            )
            == 8
        )
        assert b"".join(chunks) == b"abcdefgh"
    else:
        files = (
            fs.open_many([OpenFile(fs, "/data", mode="rb")], block_size=2)
            if reader_kind == "group"
            else [fs.open("/data", "rb", cache_type="none")]
        )
        try:
            assert files[0].read(2) == b"ab"
            mutate_and_reconnect()
            assert files[0].read(6) == b"cdefgh"
        finally:
            fs.commit_many(files)
    assert not fs._client._fds


@pytest.mark.parametrize("exhausted", [False, True])
@pytest.mark.parametrize("grow", [False, True])
def test_cat_verifies_stat_zero_files_even_after_budget_is_exhausted(
    fs, monkeypatch, exhausted, grow
):
    fs.read_all_max_total_bytes = 4
    fs.batch_size = 1
    fs.pipe({"/first": b"full" if exhausted else b"a", "/empty": b""})
    original_stat = fs._client.stat_many
    original_read = fs._client.read_all_many
    reads = []

    def stat_then_grow(paths):
        stats = original_stat(paths)
        if grow:
            fs.pipe_file("/empty", b"x")
        return stats

    def record_read(paths, budget=None):
        reads.append((list(paths), budget))
        return original_read(paths, budget)

    monkeypatch.setattr(fs._client, "stat_many", stat_then_grow)
    monkeypatch.setattr(fs._client, "read_all_many", record_read)
    result = fs.cat(["/first", "/empty"], on_error="return")
    assert result["/first"] == (b"full" if exhausted else b"a")
    if exhausted and grow:
        assert isinstance(result["/empty"], OSError)
        assert result["/empty"].errno == errno.EFBIG
    else:
        assert result["/empty"] == (b"x" if grow else b"")
    assert reads == [
        ([fs._native_path("/first")], 4),
        ([fs._native_path("/empty")], 0 if exhausted else 3),
    ]


@pytest.mark.parametrize("code", [errno.EACCES, errno.EISDIR])
def test_cat_does_not_hide_read_errors_for_stat_zero_files(fs, monkeypatch, code):
    fs.pipe_file("/empty", b"")
    calls = []

    def fail_read(paths, budget=None):
        calls.append(list(paths))
        return [None], {0: code}

    monkeypatch.setattr(fs._client, "read_all_many", fail_read)
    result = fs.cat(["/empty"], on_error="return")
    assert isinstance(result["/empty"], OSError)
    assert result["/empty"].errno == code
    assert calls == [[fs._native_path("/empty")]]


@pytest.mark.parametrize("reader_kind", ["scalar", "group"])
def test_reader_recovery_rejects_same_fileid_in_different_filesystem(
    dummy_fs, monkeypatch, reader_kind
):
    fs = dummy_fs
    fs.pipe_file("/data", b"abcdef")
    if reader_kind == "scalar":
        reader = fs.open("/data", "rb", cache_type="none")
    else:
        reader = fs.open_many([OpenFile(fs, "/data", mode="rb")])[0]
    try:
        if reader_kind == "scalar":
            assert reader.read(1) == b"a"
            fs._client.reconnect()
        fstat = fs._client.fstat

        def different_fs(fd):
            attrs = dict(fstat(fd))
            attrs["fs_id"] += 1
            return attrs

        monkeypatch.setattr(fs._client, "fstat", different_fs)
        if reader_kind == "group":
            monkeypatch.setattr(
                fs._client, "fstat_many", lambda fds: [different_fs(fd) for fd in fds]
            )

            def failed_read(*args, **kwargs):
                raise ConnectionError("injected grouped read transport failure")

            monkeypatch.setattr(fs._client, "preadv", failed_read)
        with pytest.raises(OSError) as error:
            reader.read(1)
        assert error.value.errno == errno.ESTALE
    finally:
        reader.close()


@pytest.mark.parametrize("start,end", [(1, None), (-3, None), (1, 0)])
@pytest.mark.parametrize("replacement", [b"abcdefghijklmnop", b"ab"])
def test_cat_file_freezes_checked_range_before_file_changes(
    fs, monkeypatch, start, end, replacement
):
    fs.read_all_max_total_bytes = 8
    fs.pipe_file("/range", b"abcd")
    stat_many = fs._client.stat_many

    def stat_then_change(paths):
        result = stat_many(paths)
        fs.pipe_file("/range", replacement)
        return result

    monkeypatch.setattr(fs._client, "stat_many", stat_then_change)
    assert fs.cat_file("/range", start=start, end=end) == replacement[1:4]


@pytest.mark.parametrize("batch_size", [2, 3])
@pytest.mark.parametrize("on_error", ["raise", "return"])
def test_cat_ranges_freezes_aggregate_bounds_and_keeps_vector_batches(
    fs, monkeypatch, batch_size, on_error
):
    fs.read_all_max_total_bytes = 9
    fs.batch_size = batch_size
    fs.max_batch_bytes = batch_size * 3
    paths = ["/range-a", "/range-b", "/range-c"]
    fs.pipe(dict.fromkeys(paths, b"abcd"))
    stat_many = fs._client.stat_many
    read_many = fs._client.read_many
    batches = []

    def stat_then_grow(native_paths):
        result = stat_many(native_paths)
        fs.pipe(dict.fromkeys(paths, b"abcdefghijklmnop"))
        return result

    def record_read(native_paths, starts, ends):
        batches.append((list(starts), list(ends)))
        return read_many(native_paths, starts, ends)

    monkeypatch.setattr(fs._client, "stat_many", stat_then_grow)
    monkeypatch.setattr(fs._client, "read_many", record_read)
    result = fs.cat_ranges(paths, [1, -3, 1], [None, None, 0], on_error=on_error)
    assert result == [b"bcd"] * 3
    assert sum(map(len, result)) == fs.read_all_max_total_bytes
    widths = [2, 1] if batch_size == 2 else [3]
    assert batches == [([1] * width, [4] * width) for width in widths]


@pytest.mark.parametrize(
    "api,start", [("cat_file", 1), ("cat_ranges", 0), ("cat_ranges", 1)]
)
def test_open_ended_range_with_zero_budget_excludes_later_growth(
    fs, monkeypatch, api, start
):
    fs.read_all_max_total_bytes = 0
    fs.pipe_file("/empty-range", b"")
    stat_many = fs._client.stat_many

    def stat_then_grow(paths):
        result = stat_many(paths)
        fs.pipe_file("/empty-range", b"later")
        return result

    monkeypatch.setattr(fs._client, "stat_many", stat_then_grow)
    if api == "cat_file":
        assert fs.cat_file("/empty-range", start=start) == b""
    else:
        assert fs.cat_ranges(["/empty-range"], [start], [None]) == [b""]


@pytest.mark.parametrize("on_error", ["return", "raise"])
def test_bounded_range_growth_preserves_missing_path_errors(fs, monkeypatch, on_error):
    fs.read_all_max_total_bytes = 3
    fs.pipe_file("/range", b"abcd")
    stat_many = fs._client.stat_many

    def stat_then_grow(paths):
        result = stat_many(paths)
        fs.pipe_file("/range", b"abcdefghijklmnop")
        return result

    monkeypatch.setattr(fs._client, "stat_many", stat_then_grow)
    paths = ["/range", "/missing-range"]
    if on_error == "raise":
        with pytest.raises(FileNotFoundError):
            fs.cat_ranges(paths, 1, None, on_error=on_error)
    else:
        result = fs.cat_ranges(paths, 1, None, on_error=on_error)
        assert result[0] == b"bcd"
        assert isinstance(result[1], FileNotFoundError)
