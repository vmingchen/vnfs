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


def test_pipelined_stream_opens_worker_sessions_concurrently(fs, monkeypatch):
    payload = b"parallel-open"
    fs.pipe_file("/data", payload)
    barrier = threading.Barrier(3)

    for client in fs._client._clients:
        original = client.open

        def synchronized_open(path, mode, _open=original):
            barrier.wait(timeout=2)
            return _open(path, mode)

        monkeypatch.setattr(client, "open", synchronized_open)

    chunks = []
    assert fs.read_stream_pipelined(
        "/data",
        lambda offset, data: chunks.append((offset, data)),
        workers=3,
        chunk_size=4,
    ) == len(payload)
    assert b"".join(data for _, data in chunks) == payload


def test_pipelined_stream_open_failure_closes_successful_siblings(fs, monkeypatch):
    fs.pipe_file("/data", b"opened")
    failed_client = fs._client._clients[1]

    def fail_open(_path, _mode):
        raise PermissionError("injected OPEN failure")

    monkeypatch.setattr(failed_client, "open", fail_open)
    with pytest.raises(PermissionError, match="injected OPEN failure"):
        fs.read_stream_pipelined("/data", lambda *_: None, workers=3, chunk_size=2)
    assert fs._client._fds == {}


def test_pipelined_stream_never_overlaps_reads_on_one_descriptor(fs, monkeypatch):
    payload = b"abcdefghijklmnopqrstuvwx"
    fs.pipe_file("/data", payload)
    original = fs._client.pread
    first_started = threading.Event()
    later_started = threading.Event()
    lock = threading.Lock()
    active = {}
    peak_per_descriptor = 0

    def monitored(fd, length, offset):
        nonlocal peak_per_descriptor
        with lock:
            active[fd] = active.get(fd, 0) + 1
            peak_per_descriptor = max(peak_per_descriptor, active[fd])
        try:
            if offset == 0:
                first_started.set()
                later_started.wait(0.5)
            elif offset == 4:
                assert first_started.wait(2)
            elif offset == 8:
                later_started.set()
            return original(fd, length, offset)
        finally:
            with lock:
                active[fd] -= 1

    monkeypatch.setattr(fs._client, "pread", monitored)
    chunks = []
    assert fs.read_stream_pipelined(
        "/data",
        lambda offset, data: chunks.append((offset, data)),
        workers=2,
        chunk_size=4,
        max_in_flight=8,
        max_buffered_bytes=32,
    ) == len(payload)
    assert b"".join(data for _, data in chunks) == payload
    assert peak_per_descriptor == 1


def test_pipelined_stream_budget_counts_active_workers(fs):
    fs.pipe_file("/data", b"abcdefgh")
    chunks = []
    assert (
        fs.read_stream_pipelined(
            "/data",
            lambda offset, data: chunks.append((offset, data)),
            workers=2,
            chunk_size=4,
            max_in_flight=8,
            max_buffered_bytes=8,
        )
        == 8
    )
    assert chunks == [(0, b"abcd"), (4, b"efgh")]


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


def test_pipelined_stream_recovers_one_dropped_read(fs, monkeypatch):
    payload = b"abcdefghij" * 8
    fs.pipe_file("/data", payload)
    original = fs._client.pread
    failures = []
    reconnects = []
    original_reconnect = fs._client.reconnect_descriptor

    def dropped_once(fd, length, offset):
        if offset >= 10 and not failures:
            failures.append((fd, offset))
            raise ConnectionError("dropped read reply")
        return original(fd, length, offset)

    def record_reconnect(fd):
        reconnects.append(fd)
        return original_reconnect(fd)

    monkeypatch.setattr(fs._client, "pread", dropped_once)
    monkeypatch.setattr(fs._client, "reconnect_descriptor", record_reconnect)
    chunks = []
    assert fs.read_stream_pipelined(
        "/data",
        lambda offset, data: chunks.append((offset, data)),
        workers=3,
        chunk_size=10,
        max_in_flight=8,
        max_buffered_bytes=80,
    ) == len(payload)
    assert b"".join(data for _, data in chunks) == payload
    assert [offset for offset, _ in chunks] == list(range(0, len(payload), 10))
    assert len(failures) == len(reconnects) == 1
    assert reconnects[0] == failures[0][0]


def test_pipelined_stream_does_not_retry_callback_connection_error(fs, monkeypatch):
    fs.pipe_file("/data", b"abcdefgh")
    reconnects = []
    monkeypatch.setattr(
        fs._client, "reconnect_descriptor", lambda fd: reconnects.append(fd)
    )

    def fail(_offset, _data):
        raise ConnectionError("callback transport failed")

    with pytest.raises(ConnectionError, match="callback transport failed"):
        fs.read_stream_pipelined("/data", fail, workers=2, chunk_size=4)
    assert reconnects == []


def test_pipelined_stream_recovers_size_probe(fs, monkeypatch):
    fs.pipe_file("/data", b"abcdefgh")
    original = fs._client.fstat
    attempts = []

    def drop_first_probe(fd):
        attempts.append(fd)
        if len(attempts) == 1:
            raise ConnectionError("dropped size reply")
        return original(fd)

    monkeypatch.setattr(fs._client, "fstat", drop_first_probe)
    chunks = []
    assert (
        fs.read_stream_pipelined(
            "/data", lambda offset, data: chunks.append((offset, data)), chunk_size=4
        )
        == 8
    )
    assert chunks == [(0, b"abcd"), (4, b"efgh")]
    # The first descriptor retries its dropped probe; every worker then gets
    # its own successful identity probe.
    assert len(attempts) == 4
    assert attempts[0] == attempts[1]
    assert len(set(attempts[1:])) == 3


def test_pipelined_stream_respects_disabled_reconnect(fs, monkeypatch):
    fs.pipe_file("/data", b"abcdefgh")
    fs.auto_reconnect = False
    reconnects = []
    monkeypatch.setattr(
        fs._client, "reconnect_descriptor", lambda fd: reconnects.append(fd)
    )

    def fail_read(*_args):
        raise ConnectionError("dropped read reply")

    monkeypatch.setattr(fs._client, "pread", fail_read)
    with pytest.raises(ConnectionError, match="dropped read reply"):
        fs.read_stream_pipelined("/data", lambda *_: None, chunk_size=4)
    assert reconnects == []


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
