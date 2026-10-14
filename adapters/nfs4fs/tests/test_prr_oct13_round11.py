"""PRR round 11: failed cache replacement cannot orphan retired generations."""

import builtins
import errno
from pathlib import Path

import fsspec
import pytest
from fsspec.core import OpenFile
from vfsi_fsspec import _blockcache


@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize("failure", ["open", "truncate", "metadata"])
@pytest.mark.parametrize("held_reader", [False, True])
def test_failed_generation_replacement_retains_old_storage(
    dummy_fs, tmp_path, monkeypatch, grouped, failure, held_reader
):
    paths = ["/a", "/b"] if grouped else ["/a"]
    cached = fsspec.filesystem(
        "blockcache",
        fs=dummy_fs,
        cache_storage=str(tmp_path / "cache"),
        check_files=False,
        expiry_time=0,
        cache_check=0,
        skip_instance_cache=True,
    )
    for path in paths:
        dummy_fs.pipe_file(path, b"original")
        with cached.open(path, "rb") as file:
            assert file.read() == b"original"
    old = {
        path: _blockcache._backing_path(cached, cached._metadata.cached_files[-1][path])
        for path in paths
    }
    held = cached.open(paths[0], "rb") if held_reader else None
    for path in paths:
        dummy_fs.pipe_file(path, b"replacement")
    previous = {
        path: dict(detail) for path, detail in cached._metadata.cached_files[-1].items()
    }
    primary = OSError(errno.ENOSPC, "cache generation setup failed")
    original = builtins.open
    allocations = []

    class FailedTruncate:
        def __init__(self, file):
            self.file = file

        def __enter__(self):
            return self

        def __exit__(self, *args):
            self.file.close()

        def truncate(self, size):
            raise primary

    def fail_open(path, *args, **kwargs):
        allocating = (
            str(path).startswith(str(tmp_path / "cache" / "vfsi-"))
            and args
            and args[0] in ("wb", "xb")
        )
        if allocating:
            allocations.append(str(path))
            if len(allocations) == len(paths) and failure == "open":
                raise primary
        file = original(path, *args, **kwargs)
        if allocating and len(allocations) == len(paths) and failure == "truncate":
            return FailedTruncate(file)
        return file

    def fail_metadata(*args):
        raise primary

    with monkeypatch.context() as patch:
        patch.setattr(builtins, "open", fail_open)
        if failure == "metadata":
            patch.setattr(_blockcache, "_save_writable_metadata", fail_metadata)
        with pytest.raises(OSError) as error:
            if grouped:
                cached.open_many([OpenFile(cached, path) for path in paths])
            else:
                cached.open(paths[0], "rb")
        assert error.value is primary
        assert cached._metadata.cached_files[-1] == previous
        assert all(Path(filename).exists() for filename in old.values())
        assert set(
            str(path)
            for path in (tmp_path / "cache").iterdir()
            if path.name.startswith("vfsi-")
        ) == set(
            filename
            for filename in old.values()
            if Path(filename).name.startswith("vfsi-")
        )
    cached.load_cache()
    assert cached._metadata.cached_files[-1] == previous
    if held is not None:
        assert held.read() == b"original"
        held.close()
    if grouped:
        files = cached.open_many([OpenFile(cached, path) for path in paths])
        assert [file.read() for file in files] == [b"replacement"] * len(paths)
        for file in files:
            file.close()
    else:
        with cached.open(paths[0], "rb") as file:
            assert file.read() == b"replacement"
    assert all(not Path(filename).exists() for filename in old.values())
    assert not dummy_fs._client._fds


def test_rollback_unlink_failure_preserves_primary_and_attempts_every_file(
    dummy_fs, tmp_path, monkeypatch
):
    dummy_fs.pipe({"/a": b"original", "/b": b"original"})
    cached = fsspec.filesystem(
        "blockcache",
        fs=dummy_fs,
        cache_storage=str(tmp_path / "cache"),
        skip_instance_cache=True,
    )
    primary = OSError(errno.ENOSPC, "cache allocation failed")
    cleanup = OSError(errno.EACCES, "cache unlink denied")
    original = builtins.open
    remove = _blockcache.os.remove
    allocations, attempts = [], []

    class FailedTruncate:
        def __init__(self, file):
            self.file = file

        def __enter__(self):
            return self

        def __exit__(self, *args):
            self.file.close()

        def truncate(self, size):
            raise primary

    def allocate(path, *args, **kwargs):
        file = original(path, *args, **kwargs)
        if str(path).startswith(str(tmp_path / "cache" / "vfsi-")) and args == ("xb",):
            allocations.append(str(path))
            if len(allocations) == 2:
                return FailedTruncate(file)
        return file

    def fail_first_unlink(path):
        attempts.append(str(path))
        if str(path) == allocations[0]:
            raise cleanup
        return remove(path)

    with monkeypatch.context() as patch:
        patch.setattr(builtins, "open", allocate)
        patch.setattr(_blockcache.os, "remove", fail_first_unlink)
        with pytest.raises(OSError) as error:
            cached.open_many([OpenFile(cached, "/a"), OpenFile(cached, "/b")])
        assert error.value is primary
        assert error.value.__cause__ is cleanup
        assert attempts == allocations
        assert cached._metadata.cached_files[-1] == {}
        assert Path(allocations[0]).exists()
        assert not Path(allocations[1]).exists()
    remove(allocations[0])  # Release the test's deliberately denied unlink.
    assert not dummy_fs._client._fds
