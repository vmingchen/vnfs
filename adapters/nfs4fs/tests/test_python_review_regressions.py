"""Regression coverage for Python I/O contracts and bounded cache lifetimes."""

import errno
import subprocess
import sys
from array import array

import fsspec
import pytest
from fsspec.core import OpenFile


def _filesystem(root, **options):
    return fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(root),
        skip_instance_cache=True,
        **options,
    )


@pytest.mark.parametrize("grouped", [False, True])
def test_cache_generation_replacement_keeps_old_mmap_contents(tmp_path, grouped):
    with _filesystem(tmp_path / "remote") as fs:
        cached = fsspec.filesystem(
            "blockcache",
            fs=fs,
            cache_storage=str(tmp_path / "cache"),
            check_files=True,
            skip_instance_cache=True,
        )
        fs.pipe_file("/data", b"abcdefgh")
        old = cached.open("/data", "rb", block_size=4)
        assert old.read(4) == b"abcd"
        try:
            fs.pipe_file("/data", b"ABCDEFGH")
            if grouped:
                entries = [OpenFile(cached, "/data", mode="rb")]
                new = cached.open_many(entries)[0]
            else:
                new = cached.open("/data", "rb", block_size=4)
            try:
                old.seek(0)
                assert old.read(4) == b"abcd"
                assert new.read(4) == b"ABCD"
            finally:
                new.close()
        finally:
            old.close()


@pytest.mark.parametrize("grouped", [False, True])
def test_cache_shrink_cannot_sigbus_an_existing_mapping(tmp_path, grouped):
    # Isolate mmap faults so a regression cannot kill the pytest runner.
    script = """
import resource, sys, fsspec, nfs4fs
from pathlib import Path
from fsspec.core import OpenFile
resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
root = Path(sys.argv[1])
with fsspec.filesystem("nfs4", backend="dummy", dummy_root=str(root / "remote"),
                       skip_instance_cache=True) as fs:
    cached = fsspec.filesystem("blockcache", fs=fs,
        cache_storage=str(root / "cache"), check_files=True, skip_instance_cache=True)
    fs.pipe_file("/data", b"x" * 8192)
    old = cached.open("/data", "rb", block_size=4096)
    assert old.read(4096) == b"x" * 4096
    fs.pipe_file("/data", b"")
    if sys.argv[2] == "grouped":
        new = cached.open_many([OpenFile(cached, "/data", mode="rb")])[0]
    else:
        new = cached.open("/data", "rb", block_size=4096)
    with new:
        assert new.read() == b""
    old.seek(0)
    assert old.read(1) == b"x"
    old.close()
"""
    result = subprocess.run(
        [
            sys.executable,
            "-c",
            script,
            str(tmp_path),
            "grouped" if grouped else "scalar",
        ],
        capture_output=True,
        text=True,
        timeout=15,
    )
    assert result.returncode == 0, (result.returncode, result.stdout, result.stderr)


@pytest.mark.parametrize("grouped", [False, True])
def test_retired_cache_files_are_unlinked_and_duplicate_paths_share_a_generation(
    tmp_path, grouped
):
    with _filesystem(tmp_path / "remote") as fs:
        storage = tmp_path / "cache"
        cached = fsspec.filesystem(
            "blockcache",
            fs=fs,
            cache_storage=str(storage),
            check_files=True,
            skip_instance_cache=True,
        )
        previous = None
        for value in [b"first", b"second", b"third"]:
            fs.pipe_file("/data", value)
            if grouped:
                handles = cached.open_many(
                    [OpenFile(cached, "/data", mode="rb") for _ in range(2)]
                )
            else:
                handles = [cached.open("/data", "rb", block_size=4)]
            try:
                assert all(handle.read() == value for handle in handles)
            finally:
                for handle in handles:
                    handle.close()
            filename = storage / cached._metadata.cached_files[-1]["/data"]["fn"]
            assert filename.exists()
            if previous is not None:
                assert previous != filename
                assert not previous.exists()
            previous = filename


@pytest.mark.parametrize("operation", ["pipe", "put", "transaction"])
@pytest.mark.parametrize("progress", [2, 0])
def test_streamed_uploads_complete_short_writes_or_reject_zero_progress(
    tmp_path, monkeypatch, operation, progress
):
    with _filesystem(
        tmp_path / "remote", max_batch_bytes=3, transfer_chunk_size=4
    ) as fs:
        original = fs._client.pwrite

        def short_write(fd, data, offset):
            return original(fd, data[:progress], offset) if progress else 0

        monkeypatch.setattr(fs._client, "pwrite", short_write)
        payload = b"abcdefgh"
        source = tmp_path / "source"
        source.write_bytes(payload)

        def upload():
            if operation == "pipe":
                fs.pipe_file("/data", payload)
            elif operation == "put":
                fs.put(str(source), "/data")
            else:
                with fs.transaction:
                    with fs.open("/data", "wb") as handle:
                        handle.write(payload)

        if progress:
            upload()
            assert fs.cat_file("/data") == payload
        else:
            with pytest.raises(OSError) as error:
                upload()
            assert error.value.errno == errno.EIO


@pytest.mark.parametrize("mode", ["wb", "ab"])
def test_failed_buffered_flush_never_replays_a_completed_prefix(
    tmp_path, monkeypatch, mode
):
    with _filesystem(tmp_path / "remote") as fs:
        writer = fs.open("/data", mode, write_buffering=True, block_size=4)
        original = writer._raw.write
        calls = []

        def partial_then_full(data):
            calls.append(bytes(data))
            if len(calls) == 1:
                return original(data[:2])
            raise OSError(errno.ENOSPC, "injected full server")

        with monkeypatch.context() as patch:
            patch.setattr(writer._raw, "write", partial_then_full)
            with pytest.raises(OSError) as error:
                writer.write(b"abcde")
            assert error.value.errno == errno.ENOSPC
        # A failed writer may reject close, but must still release its handle
        # without submitting the already-committed prefix again.
        token = writer._fd
        with pytest.raises(OSError, match="unusable") as error:
            writer.close()
        assert error.value.errno == errno.ENOSPC
        assert writer.closed
        assert not fs._client.descriptor_valid(token)
        assert fs.cat_file("/data") == b"ab"
        writer.close()


def test_failed_writer_close_can_retry_cleanup_without_replaying(tmp_path, monkeypatch):
    with _filesystem(tmp_path / "remote") as fs:
        writer = fs.open("/data", "wb", write_buffering=True, block_size=8)
        writer.write(b"abcd")
        original_write = writer._raw.write
        writes = []

        def fail_write(data):
            writes.append(bytes(data))
            if len(writes) == 1:
                return original_write(data[:2])
            raise OSError(errno.ENOSPC, "full")

        with monkeypatch.context() as patch:
            patch.setattr(writer._raw, "write", fail_write)
            with pytest.raises(OSError):
                writer.flush()
        token = writer._fd
        with monkeypatch.context() as patch:

            def failed_close(fd):
                raise OSError(errno.EIO, "injected CLOSE failure")

            patch.setattr(fs._client, "close", failed_close)
            with pytest.raises(OSError) as error:
                writer.close()
            assert error.value.errno == errno.EIO
            assert not writer.closed
            assert fs._client.descriptor_valid(token)
        with pytest.raises(OSError, match="unusable") as error:
            writer.close()
        assert error.value.errno == errno.ENOSPC
        assert writer.closed
        assert not fs._client.descriptor_valid(token)
        assert fs.cat_file("/data") == b"ab"


@pytest.mark.parametrize("mixed", [False, True])
def test_empty_group_readers_do_not_issue_or_amplify_vector_reads(
    tmp_path, monkeypatch, mixed
):
    with _filesystem(tmp_path / "remote", batch_size=4, block_size=4) as fs:
        payloads = {
            f"/file-{index}": b"abcd" if mixed and index % 4 == 1 else b""
            for index in range(12)
        }
        fs.pipe(payloads)
        files = fs.open_many([OpenFile(fs, path, mode="rb") for path in payloads])
        requests = []
        original = fs._client.pread_many

        def record(fds, offsets, lengths):
            requests.append(list(lengths))
            return original(fds, offsets, lengths)

        monkeypatch.setattr(fs._client, "pread_many", record)
        try:
            for handle, payload in zip(files, payloads.values()):
                assert handle.read() == payload
                if not payload:
                    handle.seek(0)
                    assert handle.read() == payload
            assert all(length > 0 for wave in requests for length in wave)
            assert sum(map(len, requests)) == (3 if mixed else 0)
            assert len(requests) == (1 if mixed else 0)
        finally:
            fs.commit_many(files)


@pytest.mark.parametrize("whole", [False, True])
def test_speculative_cache_has_an_aggregate_budget(tmp_path, whole):
    with _filesystem(
        tmp_path / "remote", batch_size=2, max_batch_bytes=8, block_size=4
    ) as fs:
        paths = [f"/file-{index}" for index in range(12)]
        fs.pipe(dict.fromkeys(paths, b"abcd"))
        files = fs.open_many([OpenFile(fs, path, mode="rb") for path in paths])
        try:
            group = files[0]._buffer_group
            for index in [11, 10, 9, 8, 7]:
                assert files[index].read(-1 if whole else 1) == (
                    b"abcd" if whole else b"a"
                )
                retained = sum(map(len, group._ranges.values())) + sum(
                    map(len, group._whole_files.values())
                )
                assert retained <= fs.max_batch_bytes
            # Eviction must only sacrifice prefetch, never correctness.
            for index in range(7):
                assert files[index].read() == b"abcd"
        finally:
            fs.commit_many(files)
        assert group._speculative_bytes == 0
        assert not group._speculative_order


@pytest.mark.parametrize("mode", ["rb", "r+b"])
@pytest.mark.parametrize("view", [False, True])
def test_raw_readinto_matches_local_for_typed_buffers(tmp_path, mode, view):
    payload = b"abcdefgh"
    local_path = tmp_path / "local"
    local_path.write_bytes(payload)
    with _filesystem(tmp_path / "remote") as fs:
        fs.pipe_file("/data", payload)
        buffers = [array("I", [0, 0]) for _ in range(2)]
        with fsspec.filesystem("file").open(str(local_path), mode) as local:
            count = local.readinto(memoryview(buffers[0]) if view else buffers[0])
        with fs.open("/data", mode, cache_type="none") as remote:
            assert (
                remote.readinto(memoryview(buffers[1]) if view else buffers[1]) == count
            )
            assert buffers[1].tobytes() == buffers[0].tobytes()
            assert remote.tell() == count


@pytest.mark.parametrize("buffer", [b"abc", memoryview(bytearray(4))[::2]])
def test_raw_readinto_rejects_invalid_buffer_before_io(tmp_path, buffer):
    with _filesystem(tmp_path / "remote") as fs:
        fs.pipe_file("/data", b"abcd")
        with fs.open("/data", "rb", cache_type="none") as handle:
            with pytest.raises((TypeError, BufferError)):
                handle.readinto(buffer)
            assert handle.tell() == 0


@pytest.mark.parametrize(
    "name,value",
    [
        ("workers", 1.5),
        ("workers", True),
        ("chunk_size", 1.5),
        ("chunk_size", True),
        ("max_in_flight", 1.5),
        ("max_in_flight", True),
        ("max_buffered_bytes", float("nan")),
        ("max_buffered_bytes", float("inf")),
        ("max_buffered_bytes", True),
    ],
)
def test_pipeline_rejects_invalid_budget_before_open(
    tmp_path, monkeypatch, name, value
):
    with _filesystem(tmp_path / "remote", connection_pool_size=3) as fs:

        def unexpected_open(*args):
            pytest.fail("invalid pipeline budget must be rejected before OPEN")

        monkeypatch.setattr(fs._client, "open", unexpected_open)
        with pytest.raises(ValueError, match=name):
            fs.read_stream_pipelined("/data", lambda *_: None, **{name: value})


@pytest.mark.parametrize("cache", ["none", "readahead"])
@pytest.mark.parametrize("offset", [1.9, "1"])
def test_seek_rejects_noninteger_offsets_like_local(tmp_path, cache, offset):
    path = tmp_path / "local"
    path.write_bytes(b"abcd")
    with fsspec.filesystem("file").open(str(path), "rb") as local:
        with pytest.raises(TypeError):
            local.seek(offset)
    with _filesystem(tmp_path / "remote") as fs:
        fs.pipe_file("/data", b"abcd")
        with fs.open("/data", "rb", cache_type=cache) as remote:
            with pytest.raises(TypeError):
                remote.seek(offset)
            assert remote.tell() == 0


def test_seek_accepts_index_protocol(tmp_path):
    class Offset:
        def __index__(self):
            return 2

    with _filesystem(tmp_path / "remote") as fs:
        fs.pipe_file("/data", b"abcd")
        with fs.open("/data", "rb", cache_type="none") as handle:
            assert handle.seek(Offset()) == 2
            assert handle.read() == b"cd"
