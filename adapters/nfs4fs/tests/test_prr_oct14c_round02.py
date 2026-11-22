"""PRR: same-target mutations invalidate every persistent cache storage layer."""

import gc
import json
from pathlib import Path

import fsspec
import pytest
from fsspec.core import OpenFile
from fsspec.implementations.cached import CachingFileSystem

OLD = b"old-data"
NEW = b"new-data"


def _seed(fs, storage, paths):
    source = fsspec.filesystem(
        "blockcache",
        fs=fs,
        cache_storage=str(storage),
        check_files=False,
        cache_check=0,
        expiry_time=0,
        skip_instance_cache=True,
    )
    for path, expected in paths.items():
        with source.open(path, "rb", block_size=4) as file:
            assert file.read() == expected
    return source


def _cache(fs, storage, writable):
    return fsspec.filesystem(
        "blockcache",
        fs=fs,
        cache_storage=[str(path) for path in storage] + [str(writable)],
        check_files=False,
        cache_check=0,
        expiry_time=0,
        skip_instance_cache=True,
    )


def _read(cached, path, grouped):
    files = (
        cached.open_many([OpenFile(cached, path, "rb")])
        if grouped
        else [cached.open(path, "rb")]
    )
    try:
        return files[0].read()
    finally:
        for file in files:
            file.close()


def _snapshot(storage):
    return {
        path.relative_to(storage): path.read_bytes()
        for path in Path(storage).iterdir()
        if path.name != ".vfsi-cache.lock"
    }


@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize("operation", ["pipe", "write", "truncate", "hardlink"])
def test_mutations_invalidate_readonly_generation_after_metadata_reloads(
    dummy_fs, tmp_path, grouped, operation
):
    dummy_fs.pipe_file("/data", OLD)
    dummy_fs.hardlink("/data", "/alias")
    storage = tmp_path / "readonly"
    _seed(dummy_fs, storage, {"/data": OLD})
    gc.collect()  # The read-only wrapper is the only registered cache now.
    cached = _cache(dummy_fs, [storage], tmp_path / "writable")
    before = _snapshot(storage)
    if operation == "pipe":
        dummy_fs.pipe_file("/data", NEW)
        expected = NEW
    elif operation == "write":
        with dummy_fs.open("/data", "rb+") as writer:
            writer.write(NEW)
        expected = NEW
    elif operation == "truncate":
        with dummy_fs.open("/data", "rb+") as writer:
            writer.truncate(3)
        expected = OLD[:3]
    else:
        dummy_fs.pipe_file("/alias", NEW)
        expected = NEW
    for _ in range(2):
        cached.load_cache()
        assert _read(cached, "/data", grouped) == expected
    assert _snapshot(storage) == before
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize("operation", ["unlink", "rename"])
def test_namespace_mutation_invalidates_readonly_descendant_alias(
    dummy_fs, tmp_path, grouped, operation
):
    dummy_fs.makedirs("/real/sub")
    dummy_fs.symlink("real", "/alias")
    dummy_fs.pipe_file("/real/sub/data", OLD)
    storage = tmp_path / "readonly"
    _seed(dummy_fs, storage, {"/alias/sub/data": OLD})
    gc.collect()
    cached = _cache(dummy_fs, [storage], tmp_path / "writable")
    before = _snapshot(storage)
    if operation == "unlink":
        dummy_fs.rm("/real/sub", recursive=True)
    else:
        dummy_fs.mv("/real/sub", "/real/moved")
    for _ in range(2):
        cached.load_cache()
        with pytest.raises(FileNotFoundError):
            _read(cached, "/alias/sub/data", grouped)
    assert _snapshot(storage) == before
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("grouped", [False, True])
def test_already_open_readonly_reader_retains_old_inode(dummy_fs, tmp_path, grouped):
    dummy_fs.pipe_file("/data", OLD)
    storage = tmp_path / "readonly"
    _seed(dummy_fs, storage, {"/data": OLD})
    gc.collect()
    cached = _cache(dummy_fs, [storage], tmp_path / "writable")
    before = _snapshot(storage)
    reader = cached.open("/data", "rb")
    try:
        dummy_fs.pipe_file("/data", NEW)
        assert _read(cached, "/data", grouped) == NEW
        assert reader.read() == OLD
    finally:
        reader.close()
    assert _snapshot(storage) == before


@pytest.mark.parametrize("grouped", [False, True])
def test_all_readonly_locations_are_invalidated(dummy_fs, tmp_path, grouped):
    dummy_fs.pipe_file("/data", OLD)
    locations = [tmp_path / "first", tmp_path / "second"]
    for storage in locations:
        _seed(dummy_fs, storage, {"/data": OLD})
    gc.collect()
    cached = _cache(dummy_fs, locations, tmp_path / "writable")
    before = [_snapshot(storage) for storage in locations]
    dummy_fs.pipe_file("/data", NEW)
    assert _read(cached, "/data", grouped) == NEW
    cached.load_cache()
    assert _read(cached, "/data", grouped) == NEW
    assert [_snapshot(storage) for storage in locations] == before


@pytest.mark.parametrize("grouped", [False, True])
def test_seed_writer_invalidation_is_respected_by_readonly_lookup(
    dummy_fs, tmp_path, grouped
):
    dummy_fs.pipe_file("/data", OLD)
    storage = tmp_path / "readonly"
    source = _seed(dummy_fs, storage, {"/data": OLD})
    cached = _cache(dummy_fs, [storage], tmp_path / "writable")
    dummy_fs.pipe_file("/data", NEW)
    assert _read(cached, "/data", grouped) == NEW
    assert source._metadata.cached_files[-1]["/data"]["nfs4fs_blockcache_format"] == -1


@pytest.mark.parametrize("grouped", [False, True])
def test_unrelated_readonly_cache_stays_local(dummy_fs, tmp_path, grouped, monkeypatch):
    dummy_fs.pipe({"/data": OLD, "/stable": b"stable"})
    storage = tmp_path / "readonly"
    _seed(dummy_fs, storage, {"/data": OLD, "/stable": b"stable"})
    gc.collect()
    cached = _cache(dummy_fs, [storage], tmp_path / "writable")
    before = _snapshot(storage)
    dummy_fs.pipe_file("/data", NEW)

    def unexpected_open(*args, **kwargs):
        raise AssertionError("unchanged complete read-only entry should stay local")

    monkeypatch.setattr(dummy_fs, "_open", unexpected_open)
    cached.load_cache()
    assert _read(cached, "/stable", grouped) == b"stable"
    assert _snapshot(storage) == before


@pytest.mark.parametrize("grouped", [False, True])
def test_new_readonly_generation_can_be_used_after_invalidation(
    dummy_fs, tmp_path, grouped, monkeypatch
):
    dummy_fs.pipe_file("/data", OLD)
    storage = tmp_path / "readonly"
    source = _seed(dummy_fs, storage, {"/data": OLD})
    cached = _cache(dummy_fs, [storage], tmp_path / "writable")
    dummy_fs.pipe_file("/data", NEW)
    with source.open("/data", "rb") as file:
        assert file.read() == NEW

    def unexpected_open(*args, **kwargs):
        raise AssertionError("new complete read-only generation should stay local")

    monkeypatch.setattr(dummy_fs, "_open", unexpected_open)
    cached.load_cache()
    assert _read(cached, "/data", grouped) == NEW


@pytest.mark.parametrize("grouped", [False, True])
def test_partial_readonly_seed_fills_only_the_writable_layer(
    dummy_fs, tmp_path, grouped
):
    dummy_fs.pipe_file("/data", OLD)
    storage = tmp_path / "readonly"
    source = _seed(dummy_fs, storage, {})
    with source.open("/data", "rb", block_size=4) as file:
        assert file.read(4) == OLD[:4]
    entry = source._metadata.cached_files[-1]["/data"]
    assert entry["blocks"] == {0}
    backing = storage / entry["fn"]
    backing.chmod(0o400)
    before = _snapshot(storage)

    cached = _cache(dummy_fs, [storage], tmp_path / "writable")
    assert _read(cached, "/data", grouped) == OLD
    assert _snapshot(storage) == before
    assert cached._metadata.cached_files[-1]["/data"]["blocks"] is True


@pytest.mark.parametrize("check_files", [False, True])
def test_grouped_cache_uses_fresh_writable_generation_after_stale_seed(
    dummy_fs, tmp_path, monkeypatch, check_files
):
    dummy_fs.pipe_file("/data", OLD)
    seed = tmp_path / "seed"
    writable = tmp_path / "writable"
    _seed(dummy_fs, seed, {"/data": OLD})
    _seed(dummy_fs, writable, {"/data": OLD})
    metadata = seed / "cache"
    records = json.loads(metadata.read_text(encoding="utf-8"))
    if check_files:
        records["/data"]["uid"] = "stale"
    else:
        records["/data"]["time"] = 0
    metadata.write_text(json.dumps(records), encoding="utf-8")

    cached = fsspec.filesystem(
        "blockcache",
        fs=dummy_fs,
        cache_storage=[str(seed), str(writable)],
        check_files=check_files,
        expiry_time=3600,
        skip_instance_cache=True,
    )
    before = _snapshot(writable)
    calls = []
    info_many = dummy_fs._info_many

    def check_metadata(paths):
        calls.append(paths)
        assert check_files, "fresh writable cache entry should stay local"
        return info_many(paths)

    def unexpected_open(*args, **kwargs):
        pytest.fail("fresh writable cache entry should not reopen its source")

    monkeypatch.setattr(dummy_fs, "_info_many", check_metadata)
    monkeypatch.setattr(dummy_fs, "open_many", unexpected_open)
    assert _read(cached, "/data", grouped=True) == OLD
    assert calls == ([["/data"]] if check_files else [])
    assert _snapshot(writable) == before


def test_invalidation_hides_partial_seed_that_completes_later(tmp_path):
    def target():
        return fsspec.filesystem(
            "nfs4",
            backend="dummy",
            dummy_root=str(tmp_path / "remote"),
            skip_instance_cache=True,
        )

    with target() as seed_target, target() as consumer:
        consumer.pipe_file("/data", OLD)
        seed = tmp_path / "seed"
        source = _seed(seed_target, seed, {})
        with source.open("/data", "rb", block_size=4) as file:
            assert file.read(4) == OLD[:4]

        cached = _cache(consumer, [seed], tmp_path / "writable")
        with source.open("/data", "rb", block_size=4) as file:
            consumer.pipe_file("/replacement", NEW)
            consumer.mv("/replacement", "/data")
            assert _read(cached, "/data", grouped=True) == NEW

            # The other client still has the old inode and can complete its
            # original seed generation after the replacement.
            assert file.read() == OLD
        assert source._metadata.cached_files[-1]["/data"]["blocks"] is True
        assert _read(cached, "/data", grouped=False) == NEW
        assert _read(cached, "/data", grouped=True) == NEW


def test_cache_invalidation_preserves_load_cache_override_signature(dummy_fs, tmp_path):
    class CustomCache(CachingFileSystem):
        def load_cache(self):
            return super().load_cache()

    cached = CustomCache(
        fs=dummy_fs,
        cache_storage=str(tmp_path),
        skip_instance_cache=True,
    )
    dummy_fs.pipe_file("/data", OLD)
    assert _read(cached, "/data", grouped=False) == OLD
    dummy_fs.pipe_file("/data", NEW)
    assert _read(cached, "/data", grouped=False) == NEW
