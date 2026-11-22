"""Cross-cache coherence tests anchored to fsspec's local filesystem behavior."""

import json
import multiprocessing
import threading
from types import SimpleNamespace

import fsspec
import nfs4fs
import pytest
from fsspec.implementations.cache_metadata import CacheMetadata
from vfsi_fsspec import VfsiFileSystem, _blockcache, install_fsspec_blockcache_compat
from vfsi_fsspec._blockcache import _cache_lock


def _acquire_cache_in_child(storage, ready, acquired):
    cache = SimpleNamespace(storage=[storage])
    ready.set()
    with _cache_lock(cache):
        acquired.set()


def _read(fs, path):
    with fs.open(path, "rb", block_size=4) as file:
        return file.read()


def _persistent_fs(tmp_path):
    target = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "remote"),
        use_listings_cache=True,
        listings_expiry_time=3600,
        skip_instance_cache=True,
    )
    cached = fsspec.filesystem(
        "blockcache",
        fs=target,
        cache_storage=str(tmp_path / "cache"),
        cache_check=0,
        check_files=False,
        expiry_time=0,
        skip_instance_cache=True,
    )
    return target, cached


@pytest.mark.parametrize("reverse_registration", [False, True])
def test_persistent_cache_registers_multiple_backend_types(
    tmp_path, reverse_registration
):
    """Both protocol adapters must retain invalidation after either import order."""
    backends = [
        type(
            name,
            (VfsiFileSystem,),
            {
                "protocol": name.lower(),
                "_native_module": nfs4fs._native,
                "_supported_backends": frozenset({"dummy"}),
            },
        )
        for name in ("FirstAdapter", "SecondAdapter")
    ]
    if reverse_registration:
        backends.reverse()
    for backend in backends:
        install_fsspec_blockcache_compat(backend)

    for index, backend in enumerate(backends):
        target = backend(
            backend="dummy",
            dummy_root=str(tmp_path / f"remote-{index}"),
            skip_instance_cache=True,
        )
        cached = fsspec.filesystem(
            "blockcache",
            fs=target,
            cache_storage=str(tmp_path / f"cache-{index}"),
            cache_check=0,
            check_files=False,
            expiry_time=0,
            skip_instance_cache=True,
        )
        try:
            target.pipe_file("/file", b"old!")
            assert _read(cached, "/file") == b"old!"
            target.pipe_file("/file", b"new!")
            assert _read(cached, "/file") == b"new!"
        finally:
            cached.clear_cache()
            target.close()


def test_blockcache_lock_is_shared_across_independent_targets(tmp_path):
    first_target, first = _persistent_fs(tmp_path)
    second_target, second = _persistent_fs(tmp_path)
    started = threading.Event()
    acquired = threading.Event()

    def contender():
        started.set()
        with _cache_lock(second):
            acquired.set()

    try:
        with _cache_lock(first):
            thread = threading.Thread(target=contender)
            thread.start()
            assert started.wait(2)
            assert not acquired.wait(0.2)
        thread.join(5)
        assert not thread.is_alive()
        assert acquired.is_set()
    finally:
        first_target.close()
        second_target.close()


def test_blockcache_lock_is_shared_across_processes(tmp_path):
    storage = str(tmp_path / "cache")
    cache = SimpleNamespace(storage=[storage])
    context = multiprocessing.get_context("spawn")
    ready = context.Event()
    acquired = context.Event()
    process = context.Process(
        target=_acquire_cache_in_child, args=(storage, ready, acquired)
    )
    try:
        with _cache_lock(cache):
            process.start()
            assert ready.wait(10)
            assert not acquired.wait(0.2)
        process.join(10)
    finally:
        if process.is_alive():
            process.terminate()
            process.join(5)
    assert process.exitcode == 0
    assert acquired.is_set()


def test_blockcache_shared_storage_reloads_independent_target_metadata(tmp_path):
    first_target, first = _persistent_fs(tmp_path)
    second_target, second = _persistent_fs(tmp_path)
    try:
        first_target.pipe_file("/file", b"old")
        assert _read(first, "/file") == b"old"
        assert _read(second, "/file") == b"old"

        first_target.pipe_file("/file", b"updated")

        assert _read(second, "/file") == b"updated"
    finally:
        first_target.close()
        second_target.close()


def test_stale_open_handle_cannot_commit_blocks_into_new_generation(tmp_path):
    first_target, first = _persistent_fs(tmp_path)
    second_target, second = _persistent_fs(tmp_path)
    try:
        first_target.pipe_file("/file", b"original")
        handle = first.open("/file", "rb", block_size=4)
        assert handle.read(4) == b"orig"

        second_target.pipe_file("/file", b"replacement")
        assert _read(second, "/file") == b"replacement"
        handle.close()

        assert _read(first, "/file") == b"replacement"
    finally:
        first_target.close()
        second_target.close()


def test_local_new_opens_observe_same_filesystem_mutations(tmp_path):
    """Document the LocalFileSystem behavior used as the parity oracle."""
    fs = fsspec.filesystem("file", skip_instance_cache=True)
    original = str(tmp_path / "local-original")
    moved = str(tmp_path / "local-moved")

    fs.pipe_file(original, b"old")
    assert _read(fs, original) == b"old"
    fs.pipe_file(original, b"new-value")
    assert _read(fs, original) == b"new-value"

    fs.mv(original, moved)
    with pytest.raises(FileNotFoundError):
        _read(fs, original)
    assert _read(fs, moved) == b"new-value"

    fs.rm(moved)
    with pytest.raises(FileNotFoundError):
        _read(fs, moved)


def test_blockcache_new_open_observes_same_target_overwrite_and_fresh_listing(
    tmp_path,
):
    target, cached = _persistent_fs(tmp_path)
    try:
        target.pipe_file("/file", b"old")
        assert _read(cached, "/file") == b"old"
        assert target.ls("/")[0]["size"] == 3

        cached.pipe_file("/file", b"new-value")

        assert target.ls("/")[0]["size"] == 9
        assert _read(cached, "/file") == b"new-value"
    finally:
        cached.clear_cache()
        target.close()


def test_blockcache_new_open_observes_wrapper_delete_and_fresh_listing(tmp_path):
    target, cached = _persistent_fs(tmp_path)
    try:
        target.pipe_file("/file", b"old")
        assert _read(cached, "/file") == b"old"
        target.ls("/")

        cached.rm("/file")

        assert target.ls("/") == []
        with pytest.raises(FileNotFoundError):
            _read(cached, "/file")
    finally:
        cached.clear_cache()
        target.close()


def test_blockcache_new_open_observes_wrapper_rename(tmp_path):
    target, cached = _persistent_fs(tmp_path)
    try:
        target.pipe_file("/source", b"contents")
        assert _read(cached, "/source") == b"contents"
        target.ls("/")

        cached.mv("/source", "/destination")

        assert target.ls("/", detail=False) == ["nfs4:///destination"]
        with pytest.raises(FileNotFoundError):
            _read(cached, "/source")
        assert _read(cached, "/destination") == b"contents"
    finally:
        cached.clear_cache()
        target.close()


def test_blockcache_recursive_delete_invalidates_cached_descendants(tmp_path):
    target, cached = _persistent_fs(tmp_path)
    try:
        target.makedirs("/tree/child", exist_ok=True)
        target.pipe({"/tree/top": b"top", "/tree/child/nested": b"nested"})
        assert _read(cached, "/tree/top") == b"top"
        assert _read(cached, "/tree/child/nested") == b"nested"

        cached.rm("/tree", recursive=True)

        with pytest.raises(FileNotFoundError):
            _read(cached, "/tree/top")
        with pytest.raises(FileNotFoundError):
            _read(cached, "/tree/child/nested")
    finally:
        cached.clear_cache()
        target.close()


def test_mutation_invalidates_every_registered_blockcache_wrapper(tmp_path):
    target, first = _persistent_fs(tmp_path)
    second = fsspec.filesystem(
        "blockcache",
        fs=target,
        cache_storage=str(tmp_path / "second-cache"),
        cache_check=0,
        check_files=False,
        expiry_time=0,
        skip_instance_cache=True,
    )
    try:
        target.pipe_file("/file", b"old")
        assert _read(first, "/file") == b"old"
        assert _read(second, "/file") == b"old"

        target.pipe_file("/file", b"new")

        assert _read(first, "/file") == b"new"
        assert _read(second, "/file") == b"new"
    finally:
        first.clear_cache()
        second.clear_cache()
        target.close()


def test_invalid_generation_is_persisted_for_a_reconstructed_wrapper(tmp_path):
    target, cached = _persistent_fs(tmp_path)
    storage = str(tmp_path / "cache")
    try:
        target.pipe_file("/file", b"old")
        assert _read(cached, "/file") == b"old"

        target.pipe_file("/file", b"new")
        reconstructed = fsspec.filesystem(
            "blockcache",
            fs=target,
            cache_storage=storage,
            cache_check=0,
            check_files=False,
            expiry_time=0,
            skip_instance_cache=True,
        )
        assert _read(reconstructed, "/file") == b"new"
    finally:
        cached.clear_cache()
        target.close()


def test_blockcache_new_open_observes_update_mode_write(tmp_path):
    target, cached = _persistent_fs(tmp_path)
    try:
        target.pipe_file("/file", b"abcdefgh")
        assert _read(cached, "/file") == b"abcdefgh"

        with target.open("/file", "r+b", cache_type="none") as file:
            file.write(b"WXYZ")

        assert _read(cached, "/file") == b"WXYZefgh"
    finally:
        cached.clear_cache()
        target.close()


def test_external_mutation_requires_persistent_cache_invalidation(tmp_path):
    target, cached = _persistent_fs(tmp_path)
    external = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "remote"),
        skip_instance_cache=True,
    )
    try:
        target.pipe_file("/file", b"old")
        assert _read(cached, "/file") == b"old"

        external.pipe_file("/file", b"external")

        # fsspec's invalidate_cache() concerns directory listings. Persistent
        # file data has its own explicit eviction API.
        assert "/file" in cached._metadata.cached_files[-1]
        cached.invalidate_cache("/file")
        assert "/file" in cached._metadata.cached_files[-1]
        cached.pop_from_cache("/file")
        assert _read(cached, "/file") == b"external"
    finally:
        cached.clear_cache()
        external.close()
        target.close()


def test_existing_per_open_cache_keeps_its_buffered_blocks(tmp_path):
    target, cached = _persistent_fs(tmp_path)
    try:
        target.pipe_file("/file", b"abcdefgh")
        with cached.open("/file", "rb", block_size=4) as existing:
            assert existing.read(2) == b"ab"
            target.pipe_file("/file", b"12345678")

            existing.seek(0)
            assert existing.read(4) == b"abcd"
            assert _read(cached, "/file") == b"12345678"
    finally:
        cached.clear_cache()
        target.close()


def test_plain_per_open_cache_does_not_poison_fresh_handles_or_dircache(tmp_path):
    target = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "plain-remote"),
        use_listings_cache=True,
        listings_expiry_time=3600,
        skip_instance_cache=True,
    )
    try:
        target.pipe_file("/file", b"abcdefgh")
        with target.open("/file", "rb", block_size=4, cache_type="blockcache") as old:
            assert old.read(2) == b"ab"
            assert target.ls("/")[0]["size"] == 8

            target.pipe_file("/file", b"new-value")

            old.seek(0)
            assert old.read(4) == b"abcd"
            assert target.ls("/")[0]["size"] == 9
            assert _read(target, "/file") == b"new-value"
    finally:
        target.close()


def test_cache_expiry_refreshes_shared_metadata_and_preserves_lock(tmp_path):
    first_target, first = _persistent_fs(tmp_path)
    second_target, second = _persistent_fs(tmp_path)
    storage = tmp_path / "cache"
    lock = storage / ".vfsi-cache.lock"
    try:
        first_target.pipe_file("/file", b"cached!")
        assert _read(first, "/file") == b"cached!"
        detail = first._metadata.cached_files[-1]["/file"]
        backing = storage / detail["fn"]
        lock_inode = lock.stat().st_ino

        assert "/file" not in second._metadata.cached_files[-1]
        second.clear_expired_cache(expiry_time=3600)
        assert backing.read_bytes() == b"cached!"
        assert lock.stat().st_ino == lock_inode

        second.clear_expired_cache(expiry_time=-1)
        assert not backing.exists()
        first.load_cache()
        assert "/file" not in first._metadata.cached_files[-1]
        assert lock.stat().st_ino == lock_inode
    finally:
        first_target.close()
        second_target.close()


def test_clear_cache_keeps_shared_lock_and_open_reader(tmp_path):
    target, cached = _persistent_fs(tmp_path)
    storage = tmp_path / "cache"
    try:
        target.pipe_file("/file", b"abcdefgh")
        with cached.open("/file", "rb", block_size=4) as file:
            assert file.read(4) == b"abcd"
            lock = storage / ".vfsi-cache.lock"
            lock_inode = lock.stat().st_ino

            cached.clear_cache()
            assert list(storage.iterdir()) == [lock]
            assert lock.stat().st_ino == lock_inode
            assert file.read(4) == b"efgh"

        assert "/file" not in cached._metadata.cached_files[-1]
    finally:
        target.close()


def test_simplecache_clear_removes_same_name_entries(tmp_path):
    target, _ = _persistent_fs(tmp_path)
    storage = tmp_path / "simple"
    cached = fsspec.filesystem(
        "simplecache",
        fs=target,
        cache_storage=str(storage),
        same_names=True,
        skip_instance_cache=True,
    )
    name = ".vfsi-cache.lock"
    try:
        target.pipe_file("/" + name, b"ordinary data")
        assert _read(cached, "/" + name) == b"ordinary data"
        assert (storage / name).read_bytes() == b"ordinary data"

        cached.clear_cache()
        assert not (storage / name).exists()
    finally:
        target.close()


@pytest.mark.parametrize("target_by_protocol", [False, True])
def test_blockcache_uses_json_on_initial_load_and_reload(
    tmp_path, monkeypatch, target_by_protocol
):
    target, cached = _persistent_fs(tmp_path)
    storage = tmp_path / "cache"
    try:
        target.pipe_file("/file", b"abcdefgh")
        with cached.open("/file", "rb", block_size=4) as file:
            assert file.read(4) == b"abcd"

        def reject_upstream_loader(*args):
            pytest.fail("VFSI cache invoked fsspec's metadata loader")

        monkeypatch.setattr(CacheMetadata, "_load", reject_upstream_loader)
        target_args = (
            {
                "target_protocol": "nfs4",
                "target_options": {
                    "backend": "dummy",
                    "dummy_root": str(tmp_path / "remote"),
                    "skip_instance_cache": True,
                },
            }
            if target_by_protocol
            else {"fs": target}
        )
        reopened = fsspec.filesystem(
            "blockcache",
            cache_storage=str(storage),
            skip_instance_cache=True,
            **target_args,
        )
        try:
            assert reopened._metadata.cached_files[-1]["/file"]["blocks"] == {0}
            for invalid in ("invalid JSON\n", "[]", '{"/file":null}'):
                (storage / "cache").write_text(invalid, encoding="utf-8")
                with pytest.raises(ValueError, match="Invalid JSON metadata"):
                    reopened.load_cache()
        finally:
            if target_by_protocol:
                reopened.fs.close()
    finally:
        target.close()


def test_complete_scalar_cache_checks_source_identity_once(tmp_path, monkeypatch):
    target, cached = _persistent_fs(tmp_path)
    try:
        target.pipe_file("/file", b"cached!")
        assert _read(cached, "/file") == b"cached!"
        cached.check_files = True
        checks = []
        ukey = target.ukey

        def record(path):
            checks.append(path)
            return ukey(path)

        monkeypatch.setattr(target, "ukey", record)
        assert _read(cached, "/file") == b"cached!"
        assert checks == ["/file"]
    finally:
        target.close()


def test_warm_cached_reads_write_metadata_only_when_coverage_changes(
    tmp_path, monkeypatch
):
    from fsspec.core import OpenFile

    target, cached = _persistent_fs(tmp_path)
    try:
        target.pipe_file("/file", b"abcdefgh")
        with cached.open("/file", "rb", block_size=4) as file:
            assert file.read(4) == b"abcd"

        writes = []
        save = cached._metadata._save

        def record(*args, **kwargs):
            writes.append(True)
            return save(*args, **kwargs)

        loads = []
        load = cached._metadata.load

        def record_load():
            loads.append(True)
            return load()

        monkeypatch.setattr(cached._metadata, "_save", record)
        monkeypatch.setattr(cached._metadata, "load", record_load)
        with cached.open("/file", "rb", block_size=4) as file:
            assert file.read(4) == b"abcd"
            loads.clear()
        assert writes == []
        assert loads == []

        with cached.open("/file", "rb", block_size=4) as file:
            file.seek(4)
            assert file.read(4) == b"efgh"
        assert writes == [True]
        assert cached._metadata.cached_files[-1]["/file"]["blocks"] is True

        with cached.open_many([OpenFile(cached, "/file", mode="rb")])[0] as file:
            assert file.read() == b"abcdefgh"
        assert writes == [True]
    finally:
        target.close()


def test_cache_metadata_streams_entries_with_large_block_lists(tmp_path, monkeypatch):
    small = {"blocks": [0, 2]}
    large = {"blocks": list(range(_blockcache._MAX_FAST_JSON_BLOCKS + 1))}
    records = {"/small": small, "/large": large, "/complete": {"blocks": True}}
    encoded_whole = []

    class RecordingEncoder(json.JSONEncoder):
        def encode(self, value):
            encoded_whole.append(value)
            return super().encode(value)

    monkeypatch.setattr(_blockcache.json, "JSONEncoder", RecordingEncoder)
    filename = tmp_path / "cache"
    _blockcache._save_json_metadata(records, str(filename))

    assert json.loads(filename.read_text(encoding="utf-8")) == records
    assert any(value is small for value in encoded_whole)
    assert all(value is not large for value in encoded_whole)
