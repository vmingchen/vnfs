"""PRR round 9: all cached reads validate backing file confinement."""

import fsspec
import pytest
from fsspec.core import OpenFile
from vfsi_fsspec._blockcache import _save_writable_metadata


@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize("read_only_location", [False, True])
@pytest.mark.parametrize("kind", ["traversal", "symlink"])
def test_complete_cache_rejects_backing_path_escape(
    dummy_fs, tmp_path, grouped, read_only_location, kind
):
    dummy_fs.pipe_file("/data", b"remote!!")
    storage = tmp_path / "cache"
    cached = fsspec.filesystem(
        "blockcache",
        fs=dummy_fs,
        cache_storage=str(storage),
        check_files=False,
        skip_instance_cache=True,
    )
    with cached.open("/data", "rb", block_size=4) as file:
        assert file.read() == b"remote!!"
    outside = tmp_path / "outside"
    outside.write_bytes(b"outside!")
    detail = cached._metadata.cached_files[-1]["/data"]
    assert detail["blocks"] is True
    if kind == "traversal":
        detail["fn"] = "../outside"
    else:
        (storage / "bad-link").symlink_to(outside)
        detail["fn"] = "bad-link"
    _save_writable_metadata(cached)
    if read_only_location:
        cached = fsspec.filesystem(
            "blockcache",
            fs=dummy_fs,
            cache_storage=[str(storage), str(tmp_path / "writable")],
            check_files=False,
            skip_instance_cache=True,
        )
    files = []
    try:
        with pytest.raises(ValueError, match="unsafe path"):
            if grouped:
                files = cached.open_many([OpenFile(cached, "/data", "rb")])
            else:
                files = [cached.open("/data", "rb")]
        assert dummy_fs.cat_file("/data") == b"remote!!"
        assert outside.read_bytes() == b"outside!"
    finally:
        for file in files:
            file.close()


@pytest.mark.parametrize("grouped", [False, True])
def test_safe_complete_readonly_cache_remains_readable(dummy_fs, tmp_path, grouped):
    dummy_fs.pipe_file("/data", b"remote!!")
    storage = tmp_path / "readonly"
    source = fsspec.filesystem(
        "blockcache", fs=dummy_fs, cache_storage=str(storage), skip_instance_cache=True
    )
    with source.open("/data", "rb", block_size=4) as file:
        assert file.read() == b"remote!!"
    cached = fsspec.filesystem(
        "blockcache",
        fs=dummy_fs,
        cache_storage=[str(storage), str(tmp_path / "writable")],
        skip_instance_cache=True,
    )
    files = (
        cached.open_many([OpenFile(cached, "/data", "rb")])
        if grouped
        else [cached.open("/data", "rb")]
    )
    try:
        assert files[0].read() == b"remote!!"
    finally:
        for file in files:
            file.close()


@pytest.mark.parametrize("skip_first", ["expiry", "version"])
def test_scalar_cache_validates_filtered_later_location(dummy_fs, tmp_path, skip_first):
    import time

    dummy_fs.pipe_file("/data", b"remote!!")
    first_storage = tmp_path / "first"
    source = fsspec.filesystem(
        "blockcache",
        fs=dummy_fs,
        cache_storage=str(first_storage),
        skip_instance_cache=True,
    )
    with source.open("/data", "rb", block_size=4) as file:
        assert file.read() == b"remote!!"
    first = source._metadata.cached_files[-1]["/data"]
    second = dict(first)
    if skip_first == "expiry":
        first["time"] = time.time() - 1000
    else:
        first["uid"] = "another-version"
    _save_writable_metadata(source)
    cached = fsspec.filesystem(
        "blockcache",
        fs=dummy_fs,
        cache_storage=[str(first_storage), str(tmp_path / "second")],
        expiry_time=100,
        check_files=skip_first == "version",
        skip_instance_cache=True,
    )
    (tmp_path / "outside").write_bytes(b"outside!")
    second.update(fn="../outside", time=time.time())
    cached._metadata.cached_files[-1]["/data"] = second
    _save_writable_metadata(cached)
    reader = None
    try:
        with pytest.raises(ValueError, match="unsafe path"):
            reader = cached.open("/data", "rb")
    finally:
        if reader is not None:
            reader.close()
