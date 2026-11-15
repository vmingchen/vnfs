"""Bulk transfers preserve scalar file-open options and codec boundaries."""

import bz2
import errno
import gzip

import pytest
from fsspec.compression import compr


@pytest.mark.parametrize("direction", ["get", "put"])
@pytest.mark.parametrize("compression", ["gzip", "bz2", "infer"])
@pytest.mark.parametrize("oversized", [False, True])
@pytest.mark.parametrize("vector", [False, True])
def test_bulk_transfers_honor_compression(
    dummy_fs, tmp_path, direction, compression, oversized, vector
):
    fs = dummy_fs
    payloads = [b"first decoded payload" * 3, b"second decoded payload" * 3]
    codec = bz2 if compression == "bz2" else gzip
    suffix = ".bz2" if compression == "bz2" else ".gz"
    paths = [f"/file-{i}{suffix}" for i in range(2 if vector else 1)]
    locals_ = [tmp_path / f"file-{i}" for i in range(len(paths))]
    if direction == "get":
        fs.pipe({path: codec.compress(data) for path, data in zip(paths, payloads)})
    else:
        for path, data in zip(locals_, payloads):
            path.write_bytes(data)
    fs.max_batch_bytes = 8 if oversized else 1 << 20
    fs.transfer_chunk_size = 7
    remote_arg = paths if vector else paths[0]
    local_arg = [str(path) for path in locals_] if vector else str(locals_[0])
    if direction == "get":
        fs.get(remote_arg, local_arg, compression=compression)
        assert [path.read_bytes() for path in locals_] == payloads[: len(paths)]
    else:
        fs.put(local_arg, remote_arg, compression=compression)
        assert [codec.decompress(fs.cat_file(path)) for path in paths] == payloads[
            : len(paths)
        ]
    assert not fs._client._fds
    assert not fs._client._deferred_close


@pytest.mark.parametrize("direction", ["get", "put"])
@pytest.mark.parametrize(
    "defaults",
    ["empty", "compression_none", "default_nones", "literal_true", "literal_false"],
)
def test_explicit_no_compression_keeps_vector_fast_path(
    dummy_fs, tmp_path, monkeypatch, direction, defaults
):
    fs = dummy_fs
    paths = ["/a", "/b"]
    local = [tmp_path / "a", tmp_path / "b"]
    payloads = [b"aaa", b"bbb"]
    if direction == "get":
        fs.pipe(dict(zip(paths, payloads)))
    else:
        for path, data in zip(local, payloads):
            path.write_bytes(data)
    options = (
        {}
        if defaults == "empty"
        else (
            {"compression": None}
            if defaults == "compression_none"
            else dict.fromkeys(
                [
                    "compression",
                    "block_size",
                    "cache_type",
                    "cache_options",
                    "write_buffering",
                    "size",
                ]
            )
        )
    )
    if defaults.startswith("literal_"):
        options = {"assume_literal": defaults == "literal_true"}
    method = "read_all_many" if direction == "get" else "write_many"
    original = getattr(fs._client, method)
    vectors = []

    def record(*args, **kwargs):
        vectors.append(args[0])
        return original(*args, **kwargs)

    def no_scalar_open(*args, **kwargs):
        pytest.fail("default bulk transfer opened a scalar file")

    monkeypatch.setattr(fs._client, method, record)
    monkeypatch.setattr(fs._client, "open", no_scalar_open)
    if direction == "get":
        fs.get(paths, [str(path) for path in local], **options)
        assert [path.read_bytes() for path in local] == payloads
    else:
        fs.put([str(path) for path in local], paths, **options)
        assert [fs.cat_file(path) for path in paths] == payloads
    assert len(vectors) == 1
    assert len(vectors[0]) == 2


@pytest.mark.parametrize("direction", ["get", "put"])
def test_bulk_transfer_codec_errors_keep_application_provenance(
    dummy_fs, tmp_path, monkeypatch, direction
):
    fs = dummy_fs
    failure = OSError(errno.EIO, "application codec failed")
    failure.index = 0
    failure.status_domain = "application"
    locals_ = [tmp_path / "healthy", tmp_path / "bad"]
    if direction == "get":
        fs.pipe({"/healthy": b"healthy", "/bad": b"new"})
        locals_[1].write_bytes(b"old")
    else:
        locals_[0].write_bytes(b"healthy")
        locals_[1].write_bytes(b"new")
        fs.pipe_file("/bad", b"old")

    def codec(file, mode):
        if file.path.endswith("/bad"):
            raise failure
        return file

    monkeypatch.setitem(compr, "prr_application_codec", codec)
    with pytest.raises(OSError) as raised:
        if direction == "get":
            fs.get(
                ["/healthy", "/bad"],
                [str(path) for path in locals_],
                compression="prr_application_codec",
            )
        else:
            fs.put(
                [str(path) for path in locals_],
                ["/healthy", "/bad"],
                compression="prr_application_codec",
            )
    assert raised.value is failure
    assert failure.index == 0
    assert failure.status_domain == "application"
    if direction == "get":
        assert locals_[1].read_bytes() == b"old"
    else:
        # Match put_file and ordinary wb: opening precedes codec construction.
        assert fs.cat_file("/bad") == b""
    assert not fs._client._fds
    assert not fs._client._deferred_close


@pytest.mark.parametrize("compression", ["gzip", "bz2"])
def test_compressed_bulk_create_preserves_existing_destination(
    dummy_fs, tmp_path, compression
):
    local = tmp_path / "source"
    local.write_bytes(b"replacement")
    dummy_fs.pipe_file("/existing", b"retained")
    with pytest.raises(FileExistsError):
        dummy_fs.put(
            [str(local)], ["/existing"], compression=compression, mode="create"
        )
    assert dummy_fs.cat_file("/existing") == b"retained"
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("direction", ["get", "put"])
def test_bulk_transfers_forward_explicit_file_cache_options(
    dummy_fs, tmp_path, monkeypatch, direction
):
    fs = dummy_fs
    local = tmp_path / "file"
    if direction == "get":
        fs.pipe_file("/file", b"payload")
    else:
        local.write_bytes(b"payload")
    original = fs._open
    options = []

    def record(*args, **kwargs):
        options.append(
            (kwargs.get("cache_type"), kwargs.get("block_size"), kwargs.get("size"))
        )
        return original(*args, **kwargs)

    monkeypatch.setattr(fs, "_open", record)
    if direction == "get":
        fs.get(["/file"], [str(local)], cache_type="none", block_size=2, size=7)
        assert local.read_bytes() == b"payload"
    else:
        fs.put([str(local)], ["/file"], cache_type="none", block_size=2, size=7)
        assert fs.cat_file("/file") == b"payload"
    assert options == [("none", 2, 7)]
    assert not fs._client._fds


def test_bulk_cache_constructor_error_keeps_application_provenance(
    dummy_fs, tmp_path, monkeypatch
):
    from fsspec.caching import BaseCache, caches

    fs = dummy_fs
    fs.pipe({"/healthy": b"healthy", "/bad": b"new"})
    paths = [tmp_path / "healthy", tmp_path / "bad"]
    paths[1].write_bytes(b"old")
    failure = OSError(errno.EIO, "application cache failed")
    failure.index = 0
    failure.status_domain = "application"
    calls = 0

    def cache(*args, **kwargs):
        nonlocal calls
        calls += 1
        if calls == 2:
            raise failure
        return BaseCache(*args, **kwargs)

    monkeypatch.setitem(caches, "prr_application_cache", cache)
    with pytest.raises(OSError) as raised:
        fs.get(
            ["/healthy", "/bad"],
            [str(path) for path in paths],
            cache_type="prr_application_cache",
        )
    assert raised.value is failure
    assert failure.index == 0
    assert failure.status_domain == "application"
    assert paths[0].read_bytes() == b"healthy"
    assert paths[1].read_bytes() == b"old"
    assert not fs._client._fds
    assert not fs._client._deferred_close
