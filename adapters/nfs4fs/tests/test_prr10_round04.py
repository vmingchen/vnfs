"""PRR round 4: grouped finalization must retain every pending write."""

import errno

import fsspec
import pytest
from fsspec.core import OpenFile


@pytest.fixture
def fs(tmp_path):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path),
        connection_pool_size=2,
        skip_instance_cache=True,
    ) as fs:
        yield fs


def writers(fs, paths):
    files = fs.open_many(
        [OpenFile(fs, p, "wb") for p in paths], write_buffering=True, block_size=128
    )
    for file, path in zip(files, paths):
        file.write(path.encode())
    return files


def test_commit_subset_flushes_all_siblings_before_group_close(fs):
    files = writers(fs, ["/first", "/second"])
    try:
        fs.commit_many(files[:1])
        assert all(file.closed for file in files)
        assert fs.cat_file("/first") == b"/first"
        assert fs.cat_file("/second") == b"/second"
        assert not fs._client._fds
    finally:
        fs.commit_many(files)


def test_commit_combined_groups_finalizes_each_coordinator(fs):
    first = writers(fs, ["/first"])
    second = writers(fs, ["/second"])
    files = first + second
    try:
        fs.commit_many(files)
        assert all(file.closed for file in files)
        assert fs.cat_file("/first") == b"/first"
        assert fs.cat_file("/second") == b"/second"
        assert not fs._client._fds
    finally:
        for group in (first, second):
            try:
                fs.commit_many(group)
            except OSError:
                pass


def test_commit_group_failure_still_finalizes_other_groups(fs, monkeypatch):
    first = writers(fs, ["/first"])
    second = writers(fs, ["/second"])
    failure = OSError(errno.ENOSPC, "injected first group failure")

    def fail_flush(files):
        raise failure

    monkeypatch.setattr(first[0]._buffer_group, "flush_files", fail_flush)
    with pytest.raises(OSError) as error:
        fs.commit_many(first + second)
    assert error.value is failure
    assert all(file.closed for file in first + second)
    assert fs.cat_file("/second") == b"/second"
    assert not fs._client._fds
