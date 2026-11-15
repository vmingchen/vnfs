"""Persistent cache fills must use the object pinned by OPEN."""

import errno
import os

import fsspec
import pytest
from fsspec.core import OpenFile, OpenFiles


def _cached_fs(tmp_path, check_files=True):
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
