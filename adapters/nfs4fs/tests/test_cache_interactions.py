"""Cross-cache coherence tests anchored to fsspec's local filesystem behavior."""

import fsspec
import pytest


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
