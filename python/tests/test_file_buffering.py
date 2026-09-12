"""File-buffering and vectorized OpenFiles coverage."""

import io
import tempfile
import threading

import fsspec
import pytest
from fsspec.core import OpenFile, OpenFiles
from fsspec.spec import AbstractBufferedFile


def _open_files(fs, paths, mode):
    entries = [OpenFile(fs, path, mode=mode) for path in paths]
    return OpenFiles(entries, mode=mode, fs=fs)


def _record_calls(monkeypatch, target, name):
    calls = []
    original = getattr(target, name)

    def recording(*args, **kwargs):
        calls.append((args, kwargs))
        return original(*args, **kwargs)

    monkeypatch.setattr(target, name, recording)
    return calls


def test_default_read_buffer_reuses_remote_range(dummy_fs, monkeypatch):
    dummy_fs.pipe_file("/buffered", b"abcdefghijklmnopqrstuvwxyz")
    with dummy_fs.open("/buffered", "rb", block_size=8) as file:
        assert isinstance(file, AbstractBufferedFile)
        calls = _record_calls(monkeypatch, file._raw, "_pread_at")
        assert file.read(3) == b"abc"
        assert file.read(3) == b"def"
        assert len(calls) == 1
        assert calls[0][0][0] == 0
        assert calls[0][0][1] >= 8


def test_buffered_readinto_overlap_alignment_and_close(dummy_fs, monkeypatch):
    dummy_fs.pipe_file("/aligned", b"0123456789abcdef")
    file = dummy_fs.open("/aligned", "rb", block_size=4, cache_type="blockcache")
    calls = _record_calls(monkeypatch, file._raw, "_pread_at")
    file.seek(5)
    target = bytearray(2)
    assert file.readinto(target) == 2
    assert target == b"56"
    assert calls[0][0] == (4, 4)
    file.seek(4)
    assert file.read(3) == b"456"
    assert len(calls) == 1
    file.close()
    with pytest.raises(ValueError, match="closed"):
        file.read(1)


def test_none_cache_reads_directly(dummy_fs, monkeypatch):
    dummy_fs.pipe_file("/unbuffered", b"01234567")
    with dummy_fs.open("/unbuffered", "rb", cache_type="none") as file:
        calls = _record_calls(monkeypatch, file._raw, "_pread_at")
        assert file.read(2) == b"01"
        assert file.read(2) == b"23"
    assert len(calls) == 2


@pytest.mark.parametrize(
    "cache_type",
    [
        None,
        "none",
        "readahead",
        "bytes",
        "blockcache",
        "first",
        "all",
        "mmap",
        "background",
    ],
)
def test_registered_read_cache_types(dummy_fs, cache_type):
    dummy_fs.pipe_file("/cache-types", b"0123456789abcdef")
    with dummy_fs.open(
        "/cache-types", "rb", block_size=4, cache_type=cache_type
    ) as file:
        assert file.read(3) == b"012"
        assert file.seek(8) == 8
        assert file.read(4) == b"89ab"
        assert file.seek(-2, 2) == 14
        assert file.read() == b"ef"


def test_known_parts_cache_type(dummy_fs, monkeypatch):
    dummy_fs.pipe_file("/known-parts", b"0123456789abcdef")
    with dummy_fs.open(
        "/known-parts",
        "rb",
        block_size=4,
        cache_type="parts",
        cache_options={
            "data": {(0, 4): b"0123", (4, 16): b"456789abcdef"},
            "strict": True,
        },
    ) as file:
        calls = _record_calls(monkeypatch, file._raw, "_pread_at")
        assert file.read(4) == b"0123"
        assert calls == []
        assert file.read(4) == b"4567"
        assert calls == []


def test_whole_file_read_keeps_no_open_fast_path(dummy_fs):
    dummy_fs.pipe_file("/whole", b"whole file")
    with dummy_fs.open("/whole", "rb") as file:
        assert file.read() == b"whole file"
        assert file._fd is None


def test_update_modes_remain_raw(dummy_fs):
    dummy_fs.pipe_file("/update", b"abcd")
    with dummy_fs.open("/update", "r+b", block_size=2) as file:
        assert not file._using_buffer
        file.seek(1)
        file.write(b"XY")
        file.seek(0)
        assert file.read() == b"aXYd"


def test_open_files_batches_buffer_misses(dummy_fs, monkeypatch):
    paths = [f"/read-{index}" for index in range(4)]
    dummy_fs.pipe({path: bytes([65 + index]) * 16 for index, path in enumerate(paths)})
    calls = _record_calls(monkeypatch, dummy_fs._client, "pread_many")
    with _open_files(dummy_fs, paths, "rb") as files:
        for index, file in enumerate(files):
            assert file.read(4) == bytes([65 + index]) * 4
    assert len(calls) == 1
    assert len(calls[0][0][0]) == len(paths)


def test_open_files_batches_open_stat_and_close(dummy_fs, monkeypatch):
    paths = [f"/lifecycle-{index}" for index in range(4)]
    dummy_fs.pipe({path: b"data" for path in paths})
    opens = _record_calls(monkeypatch, dummy_fs._client, "open_many")
    stats = _record_calls(monkeypatch, dummy_fs._client, "fstat_many")
    closes = _record_calls(monkeypatch, dummy_fs._client, "close_many")
    with _open_files(dummy_fs, paths, "rb") as files:
        assert files[0].read(1) == b"d"
    assert [len(call[0][0]) for call in opens] == [4]
    assert [len(call[0][0]) for call in stats] == [4]
    assert [len(call[0][0]) for call in closes] == [4]


def test_group_handles_mixed_sizes_and_seek_patterns(dummy_fs, monkeypatch):
    contents = {
        "/mixed-a": b"ab",
        "/mixed-b": b"0123456789",
        "/mixed-c": b"ABCDEFGHIJKLMNO",
    }
    dummy_fs.pipe(contents)
    calls = _record_calls(monkeypatch, dummy_fs._client, "pread_many")
    with _open_files(dummy_fs, list(contents), "rb") as files:
        assert files[0].read(4) == b"ab"
        files[1].seek(8)
        assert files[1].read(4) == b"89"
        files[2].seek(4)
        assert files[2].read(3) == b"EFG"
    assert calls


def test_open_files_whole_reads_are_vectorized(dummy_fs, monkeypatch):
    paths = [f"/whole-{index}" for index in range(4)]
    expected = {path: bytes([65 + index]) * 16 for index, path in enumerate(paths)}
    dummy_fs.pipe(expected)
    calls = _record_calls(monkeypatch, dummy_fs._client, "pread_many")
    with _open_files(dummy_fs, paths, "rb") as files:
        assert [file.read() for file in files] == [expected[path] for path in paths]
    assert len(calls) == 1


def test_open_files_respects_vector_byte_cap(tmp_path, monkeypatch):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "cap"),
        block_size=4,
        max_batch_bytes=12,
        skip_instance_cache=True,
    ) as fs:
        paths = [f"/cap-{index}" for index in range(4)]
        fs.pipe({path: bytes([65 + index]) * 16 for index, path in enumerate(paths)})
        calls = _record_calls(monkeypatch, fs._client, "pread_many")
        with _open_files(fs, paths, "rb") as files:
            assert [file.read(2) for file in files] == [
                bytes([65 + index]) * 2 for index in range(4)
            ]
        assert len(calls) == 2
        assert all(sum(call[0][2]) <= 12 for call in calls)


def test_group_retains_at_most_one_speculative_block_per_sibling(tmp_path, monkeypatch):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "one-block"),
        block_size=4,
        cache_type="blockcache",
        skip_instance_cache=True,
    ) as fs:
        paths = [f"/one-block-{index}" for index in range(3)]
        fs.pipe({path: bytes([65 + index]) * 20 for index, path in enumerate(paths)})
        calls = _record_calls(monkeypatch, fs._client, "pread_many")
        with _open_files(fs, paths, "rb") as files:
            assert files[0].read(12) == b"A" * 12
            sibling_ranges = [
                key
                for key in files[0]._buffer_group._ranges
                if key[0] in {id(files[1]), id(files[2])}
            ]
            assert len(sibling_ranges) == 2
            assert all(end - start <= 4 for _, start, end in sibling_ranges)
        # Older fsspec blockcache versions fetch one extra boundary block.
        # Only the first miss may fan out; all later misses stay on the file
        # being consumed because each sibling already owns one speculative
        # block.
        assert len(calls) >= 3
        assert len(calls[0][0][0]) == 3
        assert all(len(call[0][0]) == 1 for call in calls[1:])


def test_group_whole_read_only_prefetches_one_sibling_block(tmp_path, monkeypatch):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "whole-cap"),
        block_size=4,
        skip_instance_cache=True,
    ) as fs:
        paths = [f"/whole-cap-{index}" for index in range(3)]
        fs.pipe({path: bytes([65 + index]) * 20 for index, path in enumerate(paths)})
        calls = _record_calls(monkeypatch, fs._client, "pread_many")
        with _open_files(fs, paths, "rb") as files:
            assert files[0].read() == b"A" * 20
        assert calls[0][0][2] == [20, 4, 4]


def test_vectorized_buffering_can_be_disabled(tmp_path, monkeypatch):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "disabled"),
        block_size=4,
        vectorized_buffering=False,
        skip_instance_cache=True,
    ) as fs:
        paths = [f"/disabled-{index}" for index in range(3)]
        fs.pipe({path: b"data" for path in paths})
        calls = _record_calls(monkeypatch, fs._client, "pread")
        with _open_files(fs, paths, "rb") as files:
            assert [file.read(2) for file in files] == [b"da"] * 3
        assert len(calls) == 3


def test_concurrent_group_reads_share_one_vector_fetch(dummy_fs, monkeypatch):
    paths = [f"/thread-{index}" for index in range(4)]
    dummy_fs.pipe({path: bytes([65 + index]) * 16 for index, path in enumerate(paths)})
    calls = _record_calls(monkeypatch, dummy_fs._client, "pread_many")
    with _open_files(dummy_fs, paths, "rb") as files:
        barrier = threading.Barrier(len(files))
        results = [None] * len(files)

        def read_one(index):
            barrier.wait()
            results[index] = files[index].read(4)

        threads = [
            threading.Thread(target=read_one, args=(index,))
            for index in range(len(files))
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
    assert results == [bytes([65 + index]) * 4 for index in range(4)]
    assert len(calls) == 1


def test_group_read_reconnects_and_reopens_once(dummy_fs, monkeypatch):
    paths = ["/retry-a", "/retry-b"]
    dummy_fs.pipe({path: b"retry-data" for path in paths})
    original = dummy_fs._client.pread_many
    attempts = 0

    def flaky(*args, **kwargs):
        nonlocal attempts
        attempts += 1
        if attempts == 1:
            raise ConnectionError("injected read failure")
        return original(*args, **kwargs)

    monkeypatch.setattr(dummy_fs._client, "pread_many", flaky)
    with _open_files(dummy_fs, paths, "rb") as files:
        assert [file.read(2) for file in files] == [b"re", b"re"]
    assert attempts == 2


def test_group_read_to_eof_after_speculative_prefix(dummy_fs):
    contents = {
        "/tail-primer": b"A" * 20,
        "/tail-reader": b"0123456789abcdefghij",
    }
    dummy_fs.block_size = 4
    dummy_fs.pipe(contents)
    with _open_files(dummy_fs, list(contents), "rb") as files:
        assert files[0].read(2) == b"AA"
        assert files[1].seek(2) == 2
        assert files[1].read() == contents["/tail-reader"][2:]


def test_direct_write_buffer_flushes_explicitly(tmp_path):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "direct-write"),
        write_buffering=True,
        block_size=8,
        skip_instance_cache=True,
    ) as fs:
        with fs.open("/buffered", "wb") as file:
            file.write(b"small")
            assert fs.cat_file("/buffered") == b""
            file.flush()
            assert fs.cat_file("/buffered") == b"small"
        assert fs.cat_file("/buffered") == b"small"


def test_direct_write_automatically_flushes_only_complete_blocks(tmp_path):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "direct-blocks"),
        write_buffering=True,
        block_size=4,
        skip_instance_cache=True,
    ) as fs:
        with fs.open("/buffered", "wb") as file:
            assert file.write(b"123456789") == 9
            assert fs.cat_file("/buffered") == b"12345678"
        assert fs.cat_file("/buffered") == b"123456789"


def test_direct_write_preserves_partial_block_across_calls(tmp_path):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "direct-remainder"),
        write_buffering=True,
        block_size=4,
        skip_instance_cache=True,
    ) as fs:
        with fs.open("/buffered", "wb") as file:
            file.write(b"12345")
            file.write(b"67")
        assert fs.cat_file("/buffered") == b"1234567"


def test_direct_buffered_append_starts_at_end(tmp_path):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "direct-append"),
        write_buffering=True,
        block_size=4,
        skip_instance_cache=True,
    ) as fs:
        fs.pipe_file("/buffered", b"old")
        with fs.open("/buffered", "ab") as file:
            assert file.tell() == 3
            file.write(b"-new")
            assert file.tell() == 7
        assert fs.cat_file("/buffered") == b"old-new"


def test_direct_buffered_write_is_unusable_after_transport_failure(
    tmp_path, monkeypatch
):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "direct-failure"),
        write_buffering=True,
        block_size=8,
        skip_instance_cache=True,
    ) as fs:
        attempts = 0

        def ambiguous_failure(*args, **kwargs):
            nonlocal attempts
            attempts += 1
            raise ConnectionError("injected ambiguous direct write")

        monkeypatch.setattr(fs._client, "pwrite", ambiguous_failure)
        file = fs.open("/buffered", "wb")
        file.write(b"small")
        with pytest.raises(ConnectionError, match="injected ambiguous direct write"):
            file.flush()
        with pytest.raises(ConnectionError, match="unusable"):
            file.write(b"more")
        with pytest.raises(ConnectionError, match="unusable"):
            file.close()
        assert file.closed
        assert attempts == 1


def test_open_many_closes_descriptors_when_size_discovery_fails(dummy_fs, monkeypatch):
    paths = ["/stat-failure-a", "/stat-failure-b"]
    dummy_fs.pipe({path: b"data" for path in paths})
    closes = _record_calls(monkeypatch, dummy_fs._client, "close_many")

    def fail_fstat(*args, **kwargs):
        raise OSError("injected fstat failure")

    monkeypatch.setattr(dummy_fs._client, "fstat_many", fail_fstat)
    with pytest.raises(OSError, match="injected fstat failure"):
        with _open_files(dummy_fs, paths, "rb"):
            pass
    assert len(closes) == 1
    assert len(closes[0][0][0]) == 2


def test_open_files_flushes_writes_in_vector_waves(tmp_path, monkeypatch):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "group-write"),
        write_buffering=True,
        block_size=4,
        skip_instance_cache=True,
    ) as fs:
        paths = [f"/part-{index}" for index in range(3)]
        calls = _record_calls(monkeypatch, fs._client, "pwrite_many")
        with _open_files(fs, paths, "wb") as files:
            for index, file in enumerate(files):
                file.write(bytes([65 + index]) * 9)
            assert [fs.cat_file(path) for path in paths] == [b""] * 3
        assert [fs.cat_file(path) for path in paths] == [
            bytes([65 + index]) * 9 for index in range(3)
        ]
        assert len(calls) == 3
        assert all(len(call[0][0]) == 3 for call in calls)


def test_grouped_append_uses_atomic_vector_path(tmp_path, monkeypatch):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "append"),
        write_buffering=True,
        block_size=4,
        skip_instance_cache=True,
    ) as fs:
        paths = ["/append-a", "/append-b"]
        fs.pipe({path: b"old" for path in paths})
        calls = _record_calls(monkeypatch, fs._client, "append_many")
        with _open_files(fs, paths, "ab") as files:
            for file in files:
                assert file.tell() == 3
                file.write(b"-new")
        assert [fs.cat_file(path) for path in paths] == [b"old-new"] * 2
        assert len(calls) == 1


def test_grouped_write_spills_under_aggregate_limit(tmp_path):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "spill"),
        write_buffering=True,
        block_size=8,
        max_batch_bytes=8,
        skip_instance_cache=True,
    ) as fs:
        paths = ["/spill-a", "/spill-b"]
        with _open_files(fs, paths, "wb") as files:
            files[0].write(b"1234")
            files[1].write(b"5678")
            assert any(file._write_spool._rolled for file in files)
            assert files[0]._buffer_group.in_memory_bytes() < fs.max_batch_bytes


def test_grouped_write_never_buffers_more_than_max_batch_bytes(tmp_path):
    class BoundedBuffer(io.BytesIO):
        def __init__(self, limit):
            super().__init__()
            self.limit = limit

        def write(self, data):
            if self.tell() + len(data) > self.limit:
                raise AssertionError("group buffer exceeded max_batch_bytes")
            return super().write(data)

    limit = 8
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "bounded-write"),
        write_buffering=True,
        block_size=128,
        max_batch_bytes=limit,
        skip_instance_cache=True,
    ) as fs:
        with _open_files(fs, ["/bounded"], "wb") as files:
            files[0].buffer = BoundedBuffer(limit)
            files[0].write(b"x" * (limit * 8))
        assert fs.cat_file("/bounded") == b"x" * (limit * 8)


def test_grouped_write_invalidates_repopulated_listing(tmp_path):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "listing-invalidation"),
        write_buffering=True,
        block_size=8,
        use_listings_cache=True,
        listings_expiry_time=60,
        skip_instance_cache=True,
    ) as fs:
        fs.mkdir("/dir")
        with _open_files(fs, ["/dir/x", "/dir/y"], "wb") as files:
            before = {entry["name"]: entry for entry in fs.ls("/dir", detail=True)}
            assert before["nfs4:///dir/x"]["size"] == 0
            files[0].write(b"payload")
        after = {entry["name"]: entry for entry in fs.ls("/dir", detail=True)}
        assert after["nfs4:///dir/x"]["size"] == len(b"payload")


def test_grouped_write_transport_failure_is_not_replayed(tmp_path, monkeypatch):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "failed-write"),
        write_buffering=True,
        block_size=4,
        skip_instance_cache=True,
    ) as fs:
        paths = ["/failed-a", "/failed-b"]
        attempts = 0

        def fail_once(*args, **kwargs):
            nonlocal attempts
            attempts += 1
            raise ConnectionError("injected ambiguous write failure")

        monkeypatch.setattr(fs._client, "pwrite_many", fail_once)
        opened = None
        with pytest.raises(ConnectionError, match="injected ambiguous write failure"):
            with _open_files(fs, paths, "wb") as opened:
                for file in opened:
                    file.write(b"data")
        assert attempts == 1
        assert opened is not None and all(file.closed for file in opened)


def test_descriptor_vector_primitives(dummy_fs):
    paths = ["/native-a", "/native-b"]
    fds = dummy_fs._client.open_many(paths, ["wb+", "wb+"])
    try:
        assert dummy_fs._client.pwrite_many(fds, [0, 0], [b"abc", b"xyz"]) == [
            3,
            3,
        ]
        attrs = dummy_fs._client.fstat_many(fds)
        assert [attr["size"] for attr in attrs] == [3, 3]
        data, errors = dummy_fs._client.pread_many(fds, [1, 1], [2, 2])
        assert errors == {}
        assert data == [b"bc", b"yz"]
    finally:
        dummy_fs._client.close_many(fds)


@pytest.mark.parametrize("method", ["fstat_many", "pwrite_many", "append_many"])
def test_descriptor_vector_errors_retain_failing_index(dummy_fs, method):
    fd = dummy_fs._client.open("/indexed-error", "ab+")
    try:
        arguments = {
            "fstat_many": ([fd, -1],),
            "pwrite_many": ([fd, -1], [0, 0], [b"a", b"b"]),
            "append_many": ([fd, -1], [b"a", b"b"]),
        }[method]
        with pytest.raises(OSError) as error:
            getattr(dummy_fs._client, method)(*arguments)
        assert getattr(error.value, "index", None) == 1
    finally:
        try:
            dummy_fs._client.close(fd)
        except OSError:
            pass


def test_blockcache_wrapper_accepts_nfs4fs(tmp_path, monkeypatch):
    remote_root = tempfile.mkdtemp(dir=tmp_path)
    target_options = {
        "backend": "dummy",
        "dummy_root": remote_root,
        "skip_instance_cache": True,
    }
    target = fsspec.filesystem("nfs4", **target_options)
    target.pipe_file("/wrapped", b"wrapped-data")
    target.close()
    cached = fsspec.filesystem(
        "blockcache",
        target_protocol="nfs4",
        target_options=target_options,
        cache_storage=str(tmp_path / "cache"),
        skip_instance_cache=True,
    )
    try:
        pread_calls = _record_calls(monkeypatch, cached.fs._client, "pread")
        range_calls = _record_calls(monkeypatch, cached.fs._client, "read_many")
        with cached.open("/wrapped", "rb", block_size=4) as file:
            assert file.read(4) == b"wrap"
            reads_after_miss = len(pread_calls) + len(range_calls)
            assert reads_after_miss > 0
            file.seek(0)
            assert file.read(4) == b"wrap"
            assert file.cache.blocks
        assert len(pread_calls) + len(range_calls) == reads_after_miss
    finally:
        cached.clear_cache()


def test_persistent_blockcache_open_files_populates_in_one_vector_wave(
    tmp_path, monkeypatch
):
    target = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "open-files-remote"),
        block_size=4,
        skip_instance_cache=True,
    )
    paths = [f"/cached-{index}" for index in range(4)]
    target.pipe({path: bytes([65 + index]) * 8 for index, path in enumerate(paths)})
    cached = fsspec.filesystem(
        "blockcache",
        fs=target,
        cache_storage=str(tmp_path / "open-files-cache"),
        cache_check=0,
        expiry_time=0,
        skip_instance_cache=True,
    )
    opens = _record_calls(monkeypatch, target._client, "open_many")
    reads = _record_calls(monkeypatch, target._client, "pread_many")
    closes = _record_calls(monkeypatch, target._client, "close_many")
    try:
        with _open_files(cached, paths, "rb") as files:
            assert [file.read(2) for file in files] == [
                bytes([65 + index]) * 2 for index in range(4)
            ]
            assert all(type(file.cache).__name__ == "_Nfs4MMapCache" for file in files)

        assert [len(call[0][0]) for call in opens] == [len(paths)]
        assert [len(call[0][0]) for call in reads] == [len(paths)]
        assert [len(call[0][0]) for call in closes] == [len(paths)]
        writable = cached._metadata.cached_files[-1]
        assert set(writable) == set(paths)
        assert all(writable[path]["blocks"] == {0} for path in paths)
        assert all(
            (tmp_path / "open-files-cache" / writable[path]["fn"]).exists()
            for path in paths
        )
    finally:
        cached.clear_cache()
        target.close()


def test_persistent_blockcache_open_files_complete_hits_stay_local(
    tmp_path, monkeypatch
):
    target = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "complete-remote"),
        block_size=4,
        skip_instance_cache=True,
    )
    paths = [f"/complete-{index}" for index in range(3)]
    expected = {path: bytes([97 + index]) * 4 for index, path in enumerate(paths)}
    target.pipe(expected)
    cached = fsspec.filesystem(
        "blockcache",
        fs=target,
        cache_storage=str(tmp_path / "complete-cache"),
        cache_check=0,
        expiry_time=0,
        skip_instance_cache=True,
    )
    try:
        with _open_files(cached, paths, "rb") as files:
            assert [file.read() for file in files] == [expected[path] for path in paths]

        opens = _record_calls(monkeypatch, target._client, "open_many")
        reads = _record_calls(monkeypatch, target._client, "pread_many")
        with _open_files(cached, paths, "rb") as files:
            assert [file.read() for file in files] == [expected[path] for path in paths]
        assert opens == []
        assert reads == []
    finally:
        cached.clear_cache()
        target.close()


def test_persistent_blockcache_open_files_batch_validates_new_generations(
    tmp_path, monkeypatch
):
    target = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "validation-remote"),
        block_size=4,
        skip_instance_cache=True,
    )
    paths = [f"/validated-{index}" for index in range(3)]
    original = {path: bytes([65 + index]) * 8 for index, path in enumerate(paths)}
    replacement = {path: bytes([97 + index]) * 8 for index, path in enumerate(paths)}
    target.pipe(original)
    cached = fsspec.filesystem(
        "blockcache",
        fs=target,
        cache_storage=str(tmp_path / "validation-cache"),
        cache_check=0,
        check_files=True,
        expiry_time=0,
        skip_instance_cache=True,
    )
    try:
        with _open_files(cached, paths, "rb") as files:
            assert [file.read(2) for file in files] == [
                original[path][:2] for path in paths
            ]
        target.pipe(replacement)

        stats = _record_calls(monkeypatch, target._client, "stat_many")
        reads = _record_calls(monkeypatch, target._client, "pread_many")
        with _open_files(cached, paths, "rb") as files:
            assert [file.read(2) for file in files] == [
                replacement[path][:2] for path in paths
            ]

        assert [len(call[0][0]) for call in stats] == [len(paths)]
        assert [len(call[0][0]) for call in reads] == [len(paths)]
    finally:
        cached.clear_cache()
        target.close()


def test_persistent_blockcache_open_files_writes_still_delegate(tmp_path):
    target = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "writes-remote"),
        skip_instance_cache=True,
    )
    cached = fsspec.filesystem(
        "blockcache",
        fs=target,
        cache_storage=str(tmp_path / "writes-cache"),
        skip_instance_cache=True,
    )
    paths = ["/write-a", "/write-b"]
    try:
        with _open_files(cached, paths, "wb") as files:
            files[0].write(b"alpha")
            files[1].write(b"beta")
        assert target.cat(paths) == {"/write-a": b"alpha", "/write-b": b"beta"}
    finally:
        cached.clear_cache()
        target.close()


def test_blockcache_open_files_patch_preserves_non_nfs_delegation(
    tmp_path, monkeypatch
):
    target = fsspec.filesystem("memory", skip_instance_cache=True)
    cached = fsspec.filesystem(
        "blockcache",
        fs=target,
        cache_storage=str(tmp_path / "other-target-cache"),
        skip_instance_cache=True,
    )
    opened = [object(), object()]
    committed = []
    monkeypatch.setattr(target, "open_many", lambda open_files: opened, raising=False)
    monkeypatch.setattr(
        target,
        "commit_many",
        lambda files: committed.append(files),
        raising=False,
    )

    assert cached.open_many(["first", "second"]) is opened
    cached.commit_many(opened)
    assert committed == [opened]


def test_per_open_blockcache_whole_read_populates_cache(tmp_path):
    root = tmp_path / "per-open-blockcache"
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(root),
        skip_instance_cache=True,
    ) as fs:
        fs.pipe_file("/cached", b"abcdefgh")
        with fs.open("/cached", "rb", block_size=4, cache_type="blockcache") as file:
            assert file.read() == b"abcdefgh"
            # Older supported fsspec releases also retain an empty boundary
            # block, but a whole read must populate at least the real blocks.
            assert file.cache.cache_info().currsize >= 2
            (root / "cached").write_bytes(b"WXYZ1234")
            file.seek(0)
            assert file.read() == b"abcdefgh"


def test_persistent_blockcache_does_not_mark_sparse_tail_complete(tmp_path):
    remote_root = tempfile.mkdtemp(dir=tmp_path)
    target = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=remote_root,
        skip_instance_cache=True,
    )
    target.pipe_file("/cached", b"abcdefgh")
    cached = fsspec.filesystem(
        "blockcache",
        fs=target,
        cache_storage=str(tmp_path / "persistent-tail"),
        cache_check=0,
        expiry_time=0,
        skip_instance_cache=True,
    )
    try:
        with cached.open("/cached", "rb", block_size=4) as file:
            file.seek(4)
            assert file.read(4) == b"efgh"
        with cached.open("/cached", "rb", block_size=4) as file:
            assert file.read() == b"abcdefgh"
    finally:
        cached.clear_cache()
        target.close()


@pytest.mark.parametrize(
    "replacement",
    [b"WXYZ1234", b"WXYZ01234567", b"xy"],
    ids=["same-size", "growth", "shrink"],
)
def test_persistent_blockcache_check_files_starts_a_new_generation(
    tmp_path, replacement
):
    remote_root = tempfile.mkdtemp(dir=tmp_path)
    target = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=remote_root,
        skip_instance_cache=True,
    )
    target.pipe_file("/cached", b"abcdefgh")
    cached = fsspec.filesystem(
        "blockcache",
        fs=target,
        cache_storage=str(tmp_path / "persistent-generation"),
        cache_check=0,
        check_files=True,
        expiry_time=0,
        skip_instance_cache=True,
    )
    try:
        with cached.open("/cached", "rb", block_size=4) as file:
            assert file.read(2) == b"ab"
        target.pipe_file("/cached", replacement)
        with cached.open("/cached", "rb", block_size=4) as file:
            assert file.read() == replacement
    finally:
        cached.clear_cache()
        target.close()


def test_persistent_blockcache_expiry_starts_a_new_generation(tmp_path):
    remote_root = tempfile.mkdtemp(dir=tmp_path)
    target = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=remote_root,
        skip_instance_cache=True,
    )
    target.pipe_file("/cached", b"abcdefgh")
    cached = fsspec.filesystem(
        "blockcache",
        fs=target,
        cache_storage=str(tmp_path / "persistent-expiry"),
        cache_check=0,
        expiry_time=1,
        skip_instance_cache=True,
    )
    try:
        with cached.open("/cached", "rb", block_size=4) as file:
            assert file.read(2) == b"ab"
        target.pipe_file("/cached", b"WXYZ1234")
        cached._metadata.cached_files[-1]["/cached"]["time"] = 0
        with cached.open("/cached", "rb", block_size=4) as file:
            assert file.read() == b"WXYZ1234"
    finally:
        cached.clear_cache()
        target.close()


def test_whole_read_honors_eager_all_cache(tmp_path):
    root = tmp_path / "all-cache"
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(root),
        cache_type="all",
        skip_instance_cache=True,
    ) as fs:
        fs.pipe_file("/cached", b"original")
        with fs.open("/cached", "rb") as file:
            (root / "cached").write_bytes(b"changed!")
            assert file.read() == b"original"


def test_open_files_vectorizes_eager_all_cache(tmp_path, monkeypatch):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "all-group"),
        cache_type="all",
        block_size=8,
        skip_instance_cache=True,
    ) as fs:
        paths = [f"/all-{index}" for index in range(4)]
        fs.pipe({path: bytes([65 + index]) * 4 for index, path in enumerate(paths)})
        scalar_reads = _record_calls(monkeypatch, fs._client, "pread")
        vector_reads = _record_calls(monkeypatch, fs._client, "pread_many")
        with _open_files(fs, paths, "rb") as files:
            assert [file.read(1) for file in files] == [b"A", b"B", b"C", b"D"]
        assert scalar_reads == []
        assert len(vector_reads) == 1


@pytest.mark.parametrize("fs_fixture", ["nfs_fs", "smb_fs"])
def test_persistent_blockcache_refreshes_network_generation(
    request, fs_fixture, tmp_path
):
    fs = request.getfixturevalue(fs_fixture)
    path = "/persistent-blockcache-generation"
    fs.pipe_file(path, b"abcdefgh")
    cached = fsspec.filesystem(
        "blockcache",
        fs=fs,
        cache_storage=str(tmp_path / fs_fixture),
        cache_check=0,
        check_files=True,
        expiry_time=0,
        skip_instance_cache=True,
    )
    try:
        with cached.open(path, "rb", block_size=4) as file:
            assert file.read(2) == b"ab"
        fs.pipe_file(path, b"WXYZ1234")
        with cached.open(path, "rb", block_size=4) as file:
            assert file.read() == b"WXYZ1234"
    finally:
        cached.clear_cache()


@pytest.mark.parametrize("fs_fixture", ["nfs_fs", "smb_fs"])
def test_file_buffering_smoke_on_network_backends(request, fs_fixture, tmp_path):
    fs = request.getfixturevalue(fs_fixture)
    fs.mkdir("/buffer-smoke", create_parents=True)
    read_paths = [f"/buffer-smoke/read-{index}" for index in range(4)]
    fs.pipe({path: bytes([65 + index]) * 16 for index, path in enumerate(read_paths)})

    with _open_files(fs, read_paths, "rb") as files:
        fs._client.compound_stats()
        assert [file.read(4) for file in files] == [
            bytes([65 + index]) * 4 for index in range(4)
        ]
        read_compounds = fs._client.compound_stats()[0]
        if fs.backend == "nfs":
            assert read_compounds == 1, read_compounds

    persistent = fsspec.filesystem(
        "blockcache",
        fs=fs,
        cache_storage=str(tmp_path / f"persistent-{fs_fixture}"),
        cache_check=0,
        expiry_time=0,
        skip_instance_cache=True,
    )
    try:
        with _open_files(persistent, read_paths, "rb") as files:
            fs._client.compound_stats()
            assert [file.read(4) for file in files] == [
                bytes([65 + index]) * 4 for index in range(4)
            ]
            persistent_read_compounds = fs._client.compound_stats()[0]
            if fs.backend == "nfs":
                assert persistent_read_compounds == 1, persistent_read_compounds
    finally:
        persistent.clear_cache()

    old_write_buffering = fs.write_buffering
    old_block_size = fs.block_size
    fs.write_buffering = True
    fs.block_size = 4
    try:
        write_paths = [f"/buffer-smoke/write-{index}" for index in range(4)]
        with _open_files(fs, write_paths, "wb") as files:
            for index, file in enumerate(files):
                file.write(bytes([97 + index]) * 9)
        assert fs.cat(write_paths) == {
            path: bytes([97 + index]) * 9 for index, path in enumerate(write_paths)
        }
    finally:
        fs.write_buffering = old_write_buffering
        fs.block_size = old_block_size
