"""Bounded ordered single-file read-ahead for nfs4fs."""

import threading
import time
import tracemalloc

import fsspec
import pytest


@pytest.fixture
def fs(tmp_path):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path),
        connection_pool_size=3,
        skip_instance_cache=True,
    ) as filesystem:
        yield filesystem


def test_pipelined_stream_is_ordered_and_bounded(fs, monkeypatch):
    payload = bytes(range(256)) * 8
    fs.pipe_file("/data", payload)
    original = fs._client.pread
    lock = threading.Lock()
    active = 0
    peak = 0

    def monitored(fd, length, offset):
        nonlocal active, peak
        with lock:
            active += 1
            peak = max(peak, active)
        try:
            time.sleep(0.005)
            return original(fd, length, offset)
        finally:
            with lock:
                active -= 1

    monkeypatch.setattr(fs._client, "pread", monitored)
    chunks = []
    total = fs.read_stream_pipelined(
        "/data",
        lambda offset, data: chunks.append((offset, data)),
        workers=3,
        chunk_size=128,
        max_in_flight=3,
        max_buffered_bytes=384,
    )
    assert total == len(payload)
    assert b"".join(data for _, data in chunks) == payload
    assert [offset for offset, _ in chunks] == list(range(0, len(payload), 128))
    assert 1 < peak <= 3


def test_pipelined_stream_retries_short_reads(fs, monkeypatch):
    fs.pipe_file("/data", b"abcdefghij")
    original = fs._client.pread

    def short_read(fd, length, offset):
        return original(fd, min(length, 2), offset)

    monkeypatch.setattr(fs._client, "pread", short_read)
    chunks = []
    assert (
        fs.read_stream_pipelined(
            "/data",
            lambda offset, data: chunks.append((offset, data)),
            workers=2,
            chunk_size=4,
        )
        == 10
    )
    assert chunks == [(0, b"abcd"), (4, b"efgh"), (8, b"ij")]


def test_pipelined_stream_tiny_reads_do_not_accumulate_objects(fs, monkeypatch):
    size = 64 << 10
    fs.pipe_file("/tiny", b"x" * size)
    monkeypatch.setattr(fs._client, "pread", lambda _fd, _length, _offset: b"x")
    received = []
    tracemalloc.start()
    try:
        assert (
            fs.read_stream_pipelined(
                "/tiny",
                lambda _offset, data: received.append(data),
                workers=1,
                chunk_size=size,
                max_in_flight=1,
                max_buffered_bytes=size,
            )
            == size
        )
        _, peak = tracemalloc.get_traced_memory()
    finally:
        tracemalloc.stop()
    assert received == [b"x" * size]
    assert peak < size * 6, peak


def test_pipelined_stream_closes_all_descriptors_on_callback_error(fs, monkeypatch):
    fs.pipe_file("/data", b"0123456789" * 10)
    closed = []
    original = fs._client.close

    def record_close(fd):
        closed.append(fd)
        return original(fd)

    monkeypatch.setattr(fs._client, "close", record_close)

    def fail(_offset, _data):
        raise RuntimeError("callback failed")

    with pytest.raises(RuntimeError, match="callback failed"):
        fs.read_stream_pipelined("/data", fail, workers=3, chunk_size=10)
    assert len(closed) == 3
    assert len(set(closed)) == 3


def test_pipelined_stream_validates_budget_and_closed_state(fs):
    fs.pipe_file("/data", b"x")
    with pytest.raises(ValueError, match="connection_pool_size"):
        fs.read_stream_pipelined("/data", lambda *_: None, workers=4)
    with pytest.raises(ValueError, match="1 MiB"):
        fs.read_stream_pipelined("/data", lambda *_: None, chunk_size=2 << 20)
    with pytest.raises(ValueError, match="max_buffered_bytes"):
        fs.read_stream_pipelined(
            "/data",
            lambda *_: None,
            chunk_size=128,
            max_in_flight=3,
            max_buffered_bytes=255,
        )
    fs.close()
    with pytest.raises(ValueError, match="closed"):
        fs.read_stream_pipelined("/data", lambda *_: None)


def test_pipelined_stream_uses_existing_single_session_by_default(tmp_path):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path),
        skip_instance_cache=True,
    ) as fs:
        fs.pipe_file("/data", b"one session")
        received = []
        assert fs.read_stream_pipelined(
            "/data", lambda _offset, data: received.append(data), chunk_size=4
        ) == len(b"one session")
        assert b"".join(received) == b"one session"
