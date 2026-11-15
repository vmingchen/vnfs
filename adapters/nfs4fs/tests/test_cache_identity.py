"""Persistent cache fills must use the object pinned by OPEN."""

import errno
import hashlib
import pickle
import os

import fsspec
import pytest
from fsspec.core import OpenFile, OpenFiles


def _cached_fs(tmp_path, check_files=True, **cache_options):
    target = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "remote"),
        block_size=4,
        skip_instance_cache=True,
    )
    cached = fsspec.filesystem(
        "blockcache",
        fs=target,
        cache_storage=str(tmp_path / "cache"),
        cache_check=0,
        check_files=check_files,
        expiry_time=0,
        skip_instance_cache=True,
        **cache_options,
    )
    return target, cached


@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize("check_files", [False, True])
def test_cached_generation_rejects_replacement_during_open(
    tmp_path, monkeypatch, grouped, check_files
):
    target, cached = _cached_fs(tmp_path, check_files=check_files)
    try:
        target.pipe_file("/data", b"AAAABBBB")
        with cached.open("/data", "rb", block_size=4) as file:
            assert file.read(4) == b"AAAA"
        old_identity = cached._metadata.cached_files[-1]["/data"]["identity"]
        replacement = tmp_path / "replacement"
        replacement.write_bytes(b"xxxxYYYY")
        method = "open_many" if grouped else "open"
        original = getattr(target._client, method)

        def replace_then_open(*args, **kwargs):
            os.replace(replacement, tmp_path / "remote" / "data")
            return original(*args, **kwargs)

        monkeypatch.setattr(target._client, method, replace_then_open)
        with pytest.raises(OSError) as error:
            if grouped:
                entries = OpenFiles(
                    [OpenFile(cached, "/data", mode="rb")], mode="rb", fs=cached
                )
                with entries as files:
                    files[0].read()
            else:
                with cached.open("/data", "rb", block_size=4) as file:
                    file.read()
        assert error.value.errno == errno.ESTALE
        if grouped:
            assert error.value.index == 0
        assert cached._metadata.cached_files[-1]["/data"]["identity"] == old_identity
        assert not target._client._fds
        assert not target._client._deferred_close
    finally:
        cached.clear_cache()
        target.close()


@pytest.mark.parametrize("replacement", ["replace", "unlink"])
@pytest.mark.parametrize("batch_size", [2, 128])
def test_sparse_scalar_cache_fills_use_pinned_descriptor(
    tmp_path, monkeypatch, replacement, batch_size
):
    target, cached = _cached_fs(tmp_path)
    original_data = b"AAAABBBBCCCCDDDDEEEE"
    target.batch_size = batch_size
    try:
        target.pipe_file("/data", original_data)
        with cached.open("/data", "rb", block_size=4) as file:
            for position in (4, 12):
                file.seek(position)
                assert file.read(1) == original_data[position : position + 1]
            path = tmp_path / "remote" / "data"
            if replacement == "replace":
                other = tmp_path / "replacement"
                other.write_bytes(b"xxxxYYYYzzzzWWWWkkkk")
                os.replace(other, path)
            else:
                path.unlink()
            vector_calls = []
            path_calls = []
            original_vector = target._client.pread_many
            original_paths = target._client.read_many

            def pread_many(fds, offsets, lengths):
                vector_calls.append((list(fds), list(offsets), list(lengths)))
                return original_vector(fds, offsets, lengths)

            def read_many(*args, **kwargs):
                path_calls.append(args)
                return original_paths(*args, **kwargs)

            monkeypatch.setattr(target._client, "pread_many", pread_many)
            monkeypatch.setattr(target._client, "read_many", read_many)
            file.seek(0)
            assert file.read() == original_data
            assert not path_calls
            assert len(vector_calls) == (2 if batch_size == 2 else 1)
            assert all(len(fds) <= batch_size for fds, _, _ in vector_calls)
            assert len({fd for fds, _, _ in vector_calls for fd in fds}) == 1
            assert [offset for _, offsets, _ in vector_calls for offset in offsets] == [
                0,
                8,
                16,
            ]
            assert [length for _, _, lengths in vector_calls for length in lengths] == [
                4,
                4,
                4,
            ]
        assert not target._client._fds
    finally:
        cached.clear_cache()
        target.close()


@pytest.mark.parametrize(
    "fault", ["short", "timeout", "semantic", "invalid_reply", "no_recovery"]
)
@pytest.mark.parametrize("error_channel", ["raised", "per_item"])
def test_sparse_cache_vector_faults_preserve_scalar_read_contracts(
    tmp_path, monkeypatch, fault, error_channel
):
    target, cached = _cached_fs(tmp_path)
    data = b"AAAABBBBCCCCDDDDEEEE"
    try:
        target.pipe_file("/data", data)
        with cached.open("/data", "rb", block_size=4) as file:
            for offset in (4, 12):
                file.seek(offset)
                assert file.read(1) == data[offset : offset + 1]
            original = target._client.pread_many
            if fault == "semantic":
                failure = PermissionError(errno.EACCES, "read denied")
            else:
                failure = TimeoutError("read reply lost")
                failure.transport_kind = (
                    "invalid_reply" if fault == "invalid_reply" else "timeout"
                )
            if fault == "no_recovery":
                target.auto_reconnect = False
            calls = []

            def read_many(fds, offsets, lengths):
                calls.append((offsets, lengths))
                if fault != "short" and error_channel == "raised":
                    raise failure
                values, errors = original(fds, offsets, lengths)
                if fault == "short":
                    values[0] = values[0][:2]
                else:
                    values[1] = None
                    errors[1] = failure
                return values, errors

            monkeypatch.setattr(target._client, "pread_many", read_many)
            file.seek(0)
            if fault in ("short", "timeout"):
                assert file.read() == data
            else:
                with pytest.raises(OSError) as raised:
                    file.read()
                assert raised.value is failure
            assert len(calls) == 1
        assert not target._client._fds
    finally:
        cached.clear_cache()
        target.close()


@pytest.mark.parametrize("path_bytes", [b"/plain", "/café".encode(), b"/file-\xff"])
def test_ukey_accepts_all_supported_unix_filenames(tmp_path, path_bytes):
    target, cached = _cached_fs(tmp_path)
    path = os.fsdecode(path_bytes)
    try:
        target.pipe_file(path, b"old")
        assert target.cat_file(path) == b"old"
        key = target.ukey(path)
        assert len(key) == 64
        int(key, 16)
        if path_bytes != b"/file-\xff":
            info = target.info(path)
            expected = hashlib.sha256(
                f"{info['name']}:{target._version_token(info)}".encode()
            ).hexdigest()
            assert key == expected
        target.pipe_file(path, b"changed")
        assert target.ukey(path) != key
    finally:
        cached.clear_cache()
        target.close()


@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize("mapper_kind", ["default", "basename", "custom"])
@pytest.mark.parametrize("path_bytes", [b"/plain", "/café".encode(), b"/file-\xff"])
def test_persistent_cache_accepts_all_supported_unix_filenames(
    tmp_path, grouped, mapper_kind, path_bytes
):
    from fsspec.implementations.cache_mapper import HashCacheMapper

    class CustomMapper(HashCacheMapper):
        def __call__(self, path):
            return "custom-" + hashlib.sha256(os.fsencode(path)).hexdigest()

    custom = CustomMapper()
    options = (
        {"same_names": True}
        if mapper_kind == "basename"
        else ({"cache_mapper": custom} if mapper_kind == "custom" else {})
    )
    target, cached = _cached_fs(tmp_path, **options)
    path = os.fsdecode(path_bytes)
    initial_hash = hash(cached)
    try:
        target.pipe_file(path, b"abcdefgh")
        for _ in range(2):
            if grouped:
                entries = OpenFiles(
                    [OpenFile(cached, path, mode="rb")], mode="rb", fs=cached
                )
                with entries as files:
                    assert files[0].read() == b"abcdefgh"
            else:
                with cached.open(path, "rb", block_size=4) as file:
                    assert file.read() == b"abcdefgh"
        if mapper_kind == "custom":
            assert cached._mapper is custom
        if mapper_kind == "default":
            assert hash(cached._mapper) == hash(HashCacheMapper())
            assert cached._mapper == HashCacheMapper()
            assert HashCacheMapper() == cached._mapper
            assert pickle.loads(pickle.dumps(cached._mapper)) == cached._mapper
            if path_bytes != b"/file-\xff":
                assert cached._mapper(path) == hashlib.sha256(path.encode()).hexdigest()
        assert hash(cached) == initial_hash
        assert not target._client._fds
    finally:
        cached.clear_cache()
        target.close()


@pytest.mark.parametrize("failed_position", [0, 1])
def test_published_cache_retires_all_old_files_after_unlink_error(
    tmp_path, monkeypatch, failed_position
):
    target, cached = _cached_fs(tmp_path)
    paths = ["/a", "/b"]
    try:
        target.pipe({path: b"oldbytes" for path in paths})
        entries = OpenFiles(
            [OpenFile(cached, path, mode="rb") for path in paths], mode="rb", fs=cached
        )
        with entries as files:
            assert [file.read(1) for file in files] == [b"o", b"o"]
        old = [
            tmp_path / "cache" / cached._metadata.cached_files[-1][path]["fn"]
            for path in paths
        ]
        for path in paths:
            local = tmp_path / "remote" / path.lstrip("/")
            local.write_bytes(b"newbytes")
            os.utime(local, ns=(1_000_000_000, 2_000_000_000))
        failure = PermissionError(errno.EACCES, "old generation cannot be unlinked")
        removed = []
        original_remove = os.remove

        def remove(path):
            if os.fspath(path) in {os.fspath(filename) for filename in old}:
                removed.append(os.fspath(path))
                if os.fspath(path) == os.fspath(old[failed_position]):
                    raise failure
            return original_remove(path)

        with monkeypatch.context() as patch:
            patch.setattr(os, "remove", remove)
            with pytest.raises(PermissionError) as raised:
                entries = OpenFiles(
                    [OpenFile(cached, path, mode="rb") for path in paths],
                    mode="rb",
                    fs=cached,
                )
                entries.__enter__()
            assert raised.value is failure
        assert removed == [os.fspath(filename) for filename in old]
        assert old[failed_position].exists()
        assert not old[1 - failed_position].exists()
        for path, old_filename in zip(paths, old):
            replacement = (
                tmp_path / "cache" / cached._metadata.cached_files[-1][path]["fn"]
            )
            assert replacement != old_filename
            assert replacement.exists()
        with OpenFiles(
            [OpenFile(cached, path, mode="rb") for path in paths], mode="rb", fs=cached
        ) as files:
            assert [file.read() for file in files] == [b"newbytes", b"newbytes"]
        assert not target._client._fds
    finally:
        cached.clear_cache()
        target.close()
