"""Resource, recovery, lifecycle, and concurrency regression tests."""

import math
import os
import subprocess
import sys
import threading
from concurrent.futures import ThreadPoolExecutor

import fsspec
import nfs4fs._fs as fs_module
import pytest


def _dummy(tmp_path, **kwargs):
    return fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path),
        skip_instance_cache=True,
        **kwargs,
    )


@pytest.mark.parametrize("name", ["connect_timeout", "request_timeout"])
@pytest.mark.parametrize("value", [0, -1, math.inf, math.nan, True])
def test_timeout_configuration_rejects_non_positive_or_non_finite(
    tmp_path, name, value
):
    with pytest.raises(ValueError, match=name):
        _dummy(tmp_path / "invalid", **{name: value})


def test_bulk_writes_are_bounded_by_items_and_bytes(tmp_path):
    fs = _dummy(
        tmp_path / "bounded",
        batch_size=2,
        max_batch_bytes=7,
        transfer_chunk_size=4,
    )
    calls = []
    original = fs._client.write_many

    def recording_write_many(paths, values, truncate=True):
        calls.append((len(paths), sum(len(value) for value in values)))
        return original(paths, values, truncate)

    fs._client.write_many = recording_write_many
    fs.pipe({f"/f{i}": b"abc" for i in range(5)})
    assert calls == [(2, 6), (2, 6), (1, 3)]
    assert all(items <= 2 and size <= 7 for items, size in calls)

    chunks = []
    original_pwrite = fs._client.pwrite

    def recording_pwrite(fd, data, offset):
        chunks.append(len(data))
        return original_pwrite(fd, data, offset)

    fs._client.pwrite = recording_pwrite
    fs.pipe_file("/large", b"x" * 17)
    assert chunks == [4, 4, 4, 4, 1]
    fs.close()


def test_bulk_reads_are_bounded_by_items_and_bytes(tmp_path):
    fs = _dummy(
        tmp_path / "bounded-reads",
        batch_size=2,
        max_batch_bytes=7,
        transfer_chunk_size=4,
    )
    fs.pipe({f"/f{i}": b"abc" for i in range(5)})
    calls = []
    original = fs._client.read_all_many

    def recording_read_all_many(paths):
        calls.append(len(paths))
        return original(paths)

    fs._client.read_all_many = recording_read_all_many
    result = fs.cat([f"/f{i}" for i in range(5)])
    assert result == {f"/f{i}": b"abc" for i in range(5)}
    assert calls == [2, 2, 1]

    range_calls = []
    original_ranges = fs._client.read_many

    def recording_read_many(paths, starts, ends):
        range_calls.append(len(paths))
        return original_ranges(paths, starts, ends)

    fs._client.read_many = recording_read_many
    ranges = fs.cat_ranges([f"/f{i}" for i in range(5)], 0, 3)
    assert ranges == [b"abc"] * 5
    assert range_calls == [2, 2, 1]

    fs.pipe_file("/large", b"x" * 17)
    calls.clear()
    assert fs.cat(["/large"]) == {"/large": b"x" * 17}
    assert calls == []
    fs.close()


def test_large_put_and_get_stream_in_bounded_chunks(tmp_path):
    fs = _dummy(
        tmp_path / "remote",
        max_batch_bytes=1024,
        transfer_chunk_size=257,
    )
    source = tmp_path / "source.bin"
    target = tmp_path / "target.bin"
    payload = os.urandom(4097)
    source.write_bytes(payload)

    writes = []
    original_pwrite = fs._client.pwrite

    def recording_pwrite(fd, data, offset):
        writes.append(len(data))
        return original_pwrite(fd, data, offset)

    fs._client.pwrite = recording_pwrite
    fs.put(str(source), "/large.bin")
    assert writes and max(writes) <= 257

    reads = []
    original_pread = fs._client.pread

    def recording_pread(fd, length, offset):
        reads.append(length)
        return original_pread(fd, length, offset)

    fs._client.pread = recording_pread
    fs.get("/large.bin", str(target))
    assert reads and max(reads) <= 257
    assert target.read_bytes() == payload
    fs.close()


def test_transaction_spills_to_disk_and_cleans_staging_files(tmp_path):
    fs = _dummy(tmp_path / "transaction", transaction_spool_threshold=32)
    with fs.transaction:
        file = fs.open("/large.bin", "wb")
        file.write(b"x" * 128)
        assert file._spool._rolled
        file.close()
        assert not fs.exists("/large.bin")
    assert fs.cat_file("/large.bin") == b"x" * 128
    assert not any(".nfs4fs-txn-" in path for path in fs.find("/"))
    fs.close()


def test_create_mode_is_atomic_between_independent_clients(tmp_path):
    root = tmp_path / "exclusive"
    first = _dummy(root)
    second = _dummy(root)
    barrier = threading.Barrier(2)

    def create(fs, value):
        barrier.wait()
        try:
            fs.pipe_file("/winner", value, mode="create")
            return value
        except FileExistsError:
            return None

    with ThreadPoolExecutor(max_workers=2) as pool:
        results = list(pool.map(create, (first, second), (b"one", b"two")))
    winners = [value for value in results if value is not None]
    assert len(winners) == 1
    assert first.cat_file("/winner") == winners[0]
    first.close()
    second.close()


def test_close_releases_session_and_evicts_fsspec_cache(tmp_path):
    options = {"backend": "dummy", "dummy_root": str(tmp_path / "cache")}
    first = fsspec.filesystem("nfs4", **options)
    assert fsspec.filesystem("nfs4", **options) is first
    first.close()
    assert first.closed
    with pytest.raises(ValueError, match="closed"):
        first.exists("/anything")
    second = fsspec.filesystem("nfs4", **options)
    assert second is not first
    second.close()


def test_context_manager_closes_filesystem(tmp_path):
    fs = _dummy(tmp_path / "context")
    with fs as entered:
        assert entered is fs
        fs.pipe_file("/file", b"data")
    assert fs.closed


def test_pid_change_rebuilds_session_before_use(tmp_path, monkeypatch):
    fs = _dummy(tmp_path / "fork")
    starting_generation = fs._client.generation
    parent_pid = os.getpid()
    monkeypatch.setattr(fs_module.os, "getpid", lambda: parent_pid + 1)
    assert not fs.exists("/missing")
    assert fs._client.generation == starting_generation + 1
    fs.close()


@pytest.mark.skipif(not hasattr(os, "fork"), reason="requires os.fork")
def test_inherited_filesystem_reconnects_in_child_without_harming_parent(tmp_path):
    # Fork in a fresh interpreter. Forking this long-lived pytest process after
    # RPC libraries have created helper threads is itself outside POSIX's safe
    # fork contract and can deadlock before nfs4fs gets control.
    code = r"""
import os
import sys
import fsspec
import nfs4fs

fs = fsspec.filesystem(
    "nfs4", backend="dummy", dummy_root=sys.argv[1], skip_instance_cache=True
)
fs.pipe_file("/file", b"from-parent")
read_fd, write_fd = os.pipe()
pid = os.fork()
if pid == 0:
    try:
        os.close(read_fd)
        os.write(write_fd, fs.cat_file("/file"))
    finally:
        os._exit(0)
os.close(write_fd)
assert os.read(read_fd, 64) == b"from-parent"
_, status = os.waitpid(pid, 0)
assert os.waitstatus_to_exitcode(status) == 0
assert fs.cat_file("/file") == b"from-parent"
fs.close()
"""
    subprocess.run(
        [sys.executable, "-c", code, str(tmp_path / "real-fork")],
        check=True,
        timeout=10,
    )


class _FakeNativeClient:
    instances = []
    fail_reads = True
    fail_writes = True

    def __init__(self, *args):
        self.shutdown_calls = 0
        self.abandon_calls = 0
        self.read_calls = 0
        self.write_calls = 0
        type(self).instances.append(self)

    def shutdown(self):
        self.shutdown_calls += 1

    def _abandon_after_fork(self):
        self.abandon_calls += 1

    def exists_many(self, paths):
        self.read_calls += 1
        if type(self).fail_reads:
            type(self).fail_reads = False
            raise ConnectionError("injected read failure")
        return [False] * len(paths)

    def write_many(self, paths, values, truncate=True):
        self.write_calls += 1
        if type(self).fail_writes:
            type(self).fail_writes = False
            raise ConnectionError("injected ambiguous write failure")
        return [len(value) for value in values]


def test_safe_read_reconnects_once_but_mutation_is_never_replayed(
    tmp_path, monkeypatch
):
    _FakeNativeClient.instances = []
    _FakeNativeClient.fail_reads = True
    _FakeNativeClient.fail_writes = True
    monkeypatch.setattr(fs_module._native, "NfsClient", _FakeNativeClient)
    fs = fs_module.Nfs4FileSystem(
        backend="dummy", dummy_root=str(tmp_path), skip_instance_cache=True
    )

    assert not fs.exists("/missing")
    assert len(_FakeNativeClient.instances) == 2
    assert _FakeNativeClient.instances[0].shutdown_calls == 1

    with pytest.raises(ConnectionError, match="ambiguous write"):
        fs.pipe_file("/file", b"payload")
    assert len(_FakeNativeClient.instances) == 2
    assert _FakeNativeClient.instances[-1].write_calls == 1
    fs.close()


def test_auto_reconnect_can_be_disabled(tmp_path, monkeypatch):
    _FakeNativeClient.instances = []
    _FakeNativeClient.fail_reads = True
    monkeypatch.setattr(fs_module._native, "NfsClient", _FakeNativeClient)
    fs = fs_module.Nfs4FileSystem(
        backend="dummy",
        dummy_root=str(tmp_path),
        auto_reconnect=False,
        skip_instance_cache=True,
    )
    with pytest.raises(ConnectionError, match="injected read failure"):
        fs.exists("/missing")
    assert len(_FakeNativeClient.instances) == 1
    fs.close()


@pytest.mark.skipif(sys.platform != "linux", reason="requires a POSIX FIFO")
def test_blocking_native_call_releases_the_gil(tmp_path):
    """A blocked Rust open must not prevent another Python thread running."""
    root = tmp_path / "gil"
    root.mkdir()
    fifo = root / "fifo"
    os.mkfifo(fifo)
    fs = _dummy(root)
    writer = subprocess.Popen(
        [
            sys.executable,
            "-c",
            "import sys,time; time.sleep(.25); open(sys.argv[1], 'wb').close()",
            str(fifo),
        ]
    )
    stop = threading.Event()
    counter = [0]

    def spin():
        while not stop.is_set():
            counter[0] += 1

    thread = threading.Thread(target=spin)
    thread.start()
    try:
        fd = fs._client.open("/fifo", "rb")
        fs._client.close(fd)
    finally:
        stop.set()
        thread.join(timeout=2)
        writer.wait(timeout=2)
        fs.close()
    assert counter[0] > 100
