"""Resource, recovery, lifecycle, and concurrency regression tests."""

import errno
import math
import os
import subprocess
import sys
import threading
from concurrent.futures import ThreadPoolExecutor

import fsspec
import nfs4fs._fs as fs_module
import pytest
import vfsi_fsspec._fs as engine_module


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


@pytest.mark.parametrize(
    "name",
    [
        "read_all_max_total_bytes",
        "directory_max_entries",
        "directory_max_path_bytes",
        "walk_max_depth",
    ],
)
@pytest.mark.parametrize("value", [-1, 1.5, True])
def test_allocation_limit_configuration_rejects_invalid_values(tmp_path, name, value):
    with pytest.raises(ValueError, match=name):
        _dummy(tmp_path / "invalid-limit", **{name: value})


def test_read_all_limit_is_exposed_and_survives_reconnect(tmp_path):
    fs = _dummy(tmp_path / "read-limit", read_all_max_total_bytes=4)
    assert fs.read_all_max_total_bytes == 4
    fs.pipe({"/small": b"1234", "/large": b"12345"})

    assert fs.cat_file("/small") == b"1234"
    with pytest.raises(OSError) as exc_info:
        fs.cat_file("/large")
    assert exc_info.value.errno == errno.EFBIG

    fs._client.reconnect()
    with pytest.raises(OSError) as exc_info:
        fs.cat_file("/large")
    assert exc_info.value.errno == errno.EFBIG
    fs.close()


def test_read_limit_covers_raw_buffered_range_and_aggregate_apis(tmp_path):
    fs = _dummy(tmp_path / "all-read-limits", read_all_max_total_bytes=4)
    fs.pipe({"/first": b"123456", "/second": b"abc"})

    with fs.open("/first", "rb", cache_type="none") as file:
        file.seek(1)
        with pytest.raises(OSError) as exc_info:
            file.read()
        assert exc_info.value.errno == errno.EFBIG

    with fs.open("/first", "rb", cache_type="blockcache", block_size=2) as file:
        with pytest.raises(OSError) as exc_info:
            file.read(5)
        assert exc_info.value.errno == errno.EFBIG

    with fs.open("/first", "rb", cache_type="readahead", block_size=8) as file:
        with pytest.raises(OSError) as exc_info:
            file.read(1)
        assert exc_info.value.errno == errno.EFBIG

    with pytest.raises(OSError) as exc_info:
        fs.cat_file("/first", 0, 5)
    assert exc_info.value.errno == errno.EFBIG
    with pytest.raises(OSError) as exc_info:
        fs.cat(["/first", "/second"])
    assert exc_info.value.errno == errno.EFBIG
    with pytest.raises(OSError) as exc_info:
        fs.cat_ranges(["/first", "/second"], [0, 0], [3, 3])
    assert exc_info.value.errno == errno.EFBIG
    fs.close()


def test_directory_entry_and_path_byte_limits_are_exposed(tmp_path):
    entry_limited = _dummy(tmp_path / "entry-limit", directory_max_entries=2)
    entry_limited.pipe({"/a": b"", "/b": b"", "/c": b""})
    with pytest.raises(OSError) as exc_info:
        entry_limited.ls("/")
    assert exc_info.value.errno == errno.EFBIG
    entry_limited.close()

    path_limited = _dummy(tmp_path / "path-limit", directory_max_path_bytes=2)
    path_limited.pipe_file("/long-name", b"")
    with pytest.raises(OSError) as exc_info:
        path_limited.ls("/")
    assert exc_info.value.errno == errno.EFBIG
    path_limited.close()


def test_walk_depth_limit_is_exposed(tmp_path):
    fs = _dummy(tmp_path / "walk-limit", walk_max_depth=1)
    fs.makedirs("/one/two", exist_ok=True)
    fs.pipe_file("/one/two/file", b"data")

    with pytest.raises(OSError) as exc_info:
        list(fs.walk("/"))
    assert exc_info.value.errno == errno.EFBIG
    with pytest.raises(OSError) as exc_info:
        fs.find("/")
    assert exc_info.value.errno == errno.EFBIG
    fs.close()


def test_per_call_maxdepth_stops_native_traversal_before_materialization(
    tmp_path, monkeypatch
):
    fs = _dummy(tmp_path / "shallow-walk", walk_max_depth=128)
    fs.makedirs("/one/two/three", exist_ok=True)
    fs.pipe_file("/one/two/three/file", b"data")
    calls = []
    original = fs._client.walk

    def recording_walk(root, sort=True, max_depth=None):
        calls.append(max_depth)
        return original(root, sort=sort, max_depth=max_depth)

    monkeypatch.setattr(fs._client, "walk", recording_walk)
    walked = list(fs.walk("/", maxdepth=1))
    assert [directory for directory, _, _ in walked] == ["/", "/one"]
    assert calls == [1]
    fs.close()


def test_failed_close_retains_descriptor_for_retry(tmp_path, monkeypatch):
    fs = _dummy(tmp_path / "close-retry")
    fs.pipe_file("/file", b"data")
    file = fs.open("/file", "rb", cache_type="none")
    assert file.read(1) == b"d"
    descriptor = file._fd
    original = fs._client.close
    attempts = 0

    def fail_once(fd):
        nonlocal attempts
        attempts += 1
        if attempts == 1:
            raise ConnectionError("injected close failure")
        return original(fd)

    monkeypatch.setattr(fs._client, "close", fail_once)
    with pytest.raises(ConnectionError, match="injected close failure"):
        file.close()
    assert not file.closed
    assert file._fd == descriptor
    file.close()
    assert file.closed
    assert attempts == 2
    fs.close()


def test_secure_authentication_configuration_fails_closed_without_network(tmp_path):
    with pytest.raises(ValueError, match="requires authentication"):
        fs_module.Nfs4FileSystem(
            host="unused",
            require_secure_authentication=True,
            skip_instance_cache=True,
        )
    with pytest.raises(ValueError, match="NFS-only"):
        _dummy(tmp_path / "secure", authentication="krb5i")
    with pytest.raises(ValueError, match="authentication"):
        _dummy(tmp_path / "secure", authentication="unknown")


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


def test_streaming_get_uses_read_limit_as_its_chunk_budget(tmp_path):
    fs = _dummy(
        tmp_path / "stream-read-limit",
        read_all_max_total_bytes=4,
        transfer_chunk_size=64,
    )
    payload = b"streamed-payload"
    fs.pipe_file("/large", payload)
    target = tmp_path / "streamed.bin"
    requests = []
    original = fs._client.pread

    def recording_pread(fd, length, offset):
        requests.append(length)
        return original(fd, length, offset)

    fs._client.pread = recording_pread
    fs.get("/large", str(target))
    assert target.read_bytes() == payload
    assert requests and max(requests) <= 4
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
    monkeypatch.setattr(engine_module.os, "getpid", lambda: parent_pid + 1)
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


def test_connection_pool_overlaps_independent_native_operations():
    barrier = threading.Barrier(2)

    class BlockingNative:
        def __init__(self, *args):
            pass

        def stat_many(self, paths):
            barrier.wait(timeout=2)
            return ([{"size": 0}], {})

        def shutdown(self):
            pass

    native_module = type("NativeModule", (), {"NfsClient": BlockingNative})
    pool = engine_module._ClientPool(native_module, (), size=2)
    with ThreadPoolExecutor(max_workers=2) as executor:
        futures = [executor.submit(pool.stat_many, [f"/file-{i}"]) for i in range(2)]
        assert [future.result(timeout=3) for future in futures] == [
            ([{"size": 0}], {}),
            ([{"size": 0}], {}),
        ]
    pool.shutdown()


def test_connection_pool_pins_descriptors_while_overlapping_reads():
    barrier = threading.Barrier(2)
    created = []

    class BlockingNative:
        def __init__(self, *args):
            self.identity = len(created)
            created.append(self)

        def open(self, path, mode):
            return 100 + self.identity

        def pread(self, fd, length, offset):
            assert fd == 100 + self.identity
            barrier.wait(timeout=2)
            return bytes([self.identity])

        def close(self, fd):
            assert fd == 100 + self.identity

        def shutdown(self):
            pass

    native_module = type("NativeModule", (), {"NfsClient": BlockingNative})
    pool = engine_module._ClientPool(native_module, (), size=2)
    descriptors = [pool.open(f"/file-{i}", "rb") for i in range(2)]
    with ThreadPoolExecutor(max_workers=2) as executor:
        futures = [executor.submit(pool.pread, fd, 1, 0) for fd in descriptors]
        assert {future.result(timeout=3) for future in futures} == {b"\x00", b"\x01"}
    for fd in descriptors:
        pool.close(fd)
    pool.shutdown()
