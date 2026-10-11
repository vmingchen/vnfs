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
    original_pread_many = fs._client.pread_many
    attempts = 0

    def fail_once(fds, offsets, lengths):
        nonlocal attempts
        attempts += 1
        if attempts == 1:
            raise ConnectionError("injected grouped read transport failure")
        return original_pread_many(fds, offsets, lengths)

    monkeypatch.setattr(fs._client, "pread_many", fail_once)
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
    original_pread_many = fs._client.pread_many
    attempts = 0
    vector_sizes = []

    def fail_once(fds, offsets, lengths):
        nonlocal attempts
        attempts += 1
        vector_sizes.append(len(fds))
        if attempts == 1:
            raise ConnectionError("injected grouped reader failure")
        return original_pread_many(fds, offsets, lengths)

    monkeypatch.setattr(fs._client, "pread_many", fail_once)
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


def test_cat_passes_remaining_budget_to_native_batch_when_file_grows(fs, monkeypatch):
    fs.batch_size = 1
    fs.max_batch_bytes = 2
    fs.read_all_max_total_bytes = 8
    fs.pipe({"/a": b"aa", "/b": b"bb"})
    stat_many = fs._client.stat_many
    read_all_many = fs._client.read_all_many
    grew = False
    budgets = []

    def stat_then_grow(paths):
        nonlocal grew
        stats = stat_many(paths)
        if not grew:
            grew = True
            fs.pipe_file("/b", b"x" * 8)
        return stats

    def read_with_budget(paths, max_total_bytes=None):
        budgets.append(max_total_bytes)
        if max_total_bytes is None:
            return read_all_many(paths)
        return read_all_many(paths, max_total_bytes)

    monkeypatch.setattr(fs._client, "stat_many", stat_then_grow)
    monkeypatch.setattr(fs._client, "read_all_many", read_with_budget)
    with pytest.raises(OSError) as error:
        fs.cat(["/a", "/b"])
    assert error.value.errno == errno.EFBIG
    assert budgets == [8, 6]


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
