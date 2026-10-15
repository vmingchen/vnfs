"""PRR: same-target mutations invalidate every persistent cache storage layer."""

import gc
from pathlib import Path

import fsspec
import pytest
from fsspec.core import OpenFile

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
