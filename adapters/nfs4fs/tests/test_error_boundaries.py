"""Public error semantics survive structured native failures and batching."""

import errno
import os

import fsspec
import pytest
from fsspec.core import OpenFile, OpenFiles


@pytest.mark.parametrize("exist_ok", [False, True])
def test_makedirs_dangling_symlink_matches_local(dummy_fs, tmp_path, exist_ok):
    local = fsspec.filesystem("file")
    local_path = str(tmp_path / "dangling")
    local.symlink(str(tmp_path / "missing"), local_path)
    dummy_fs.symlink("missing", "/dangling")
    with pytest.raises(FileExistsError) as expected:
        local.makedirs(local_path, exist_ok=exist_ok)
    with pytest.raises(type(expected.value)) as actual:
        dummy_fs.makedirs("/dangling", exist_ok=exist_ok)
    assert actual.value.errno == expected.value.errno == errno.EEXIST
    assert dummy_fs.readlink("/dangling") == "missing"


@pytest.mark.parametrize("method", ["cat", "cat_ranges"])
@pytest.mark.parametrize("batch_size", [1, 16])
@pytest.mark.parametrize("missing_first", [False, True])
def test_bulk_read_error_index_uses_public_input_position(
    dummy_fs, method, batch_size, missing_first
):
    dummy_fs.batch_size = batch_size
    dummy_fs.pipe_file("/ok", b"data")
    dummy_fs.mkdir("/directory")
    paths = ["/missing" if missing_first else "/ok", "/directory"]
    if method == "cat":
        results = dummy_fs.cat(paths, on_error="return")
        value, error = results[paths[0]], results[paths[1]]
        assert (
            isinstance(value, FileNotFoundError) if missing_first else value == b"data"
        )
    else:
        value, error = dummy_fs.cat_ranges(
            paths, [0, 0], [None, None], on_error="return"
        )
        assert (
            isinstance(value, FileNotFoundError) if missing_first else value == b"data"
        )
    assert isinstance(error, IsADirectoryError)
    assert error.errno == errno.EISDIR
    assert error.filename == "/directory"
    assert error.index == 1


def test_copy_retry_rebases_filtered_file_indices_without_replaying_success(
    dummy_fs, monkeypatch
):
    dummy_fs.auto_mkdir = True
    dummy_fs.mkdir("/directory")
    dummy_fs.pipe({"/ok": b"data", "/retry": b"data"})
    calls = []
    missing = FileNotFoundError(errno.ENOENT, "missing parent")
    missing.index = 1
    denied = PermissionError(errno.EACCES, "server policy")
    denied.index = 0
    denied.status_domain = "nfs"
    denied.status_code = 13

    def copy_many(pairs):
        calls.append(pairs)
        if len(calls) == 1:
            return [4, None], {1: missing}
        return [None], {0: denied}

    monkeypatch.setattr(dummy_fs._client, "copy_many", copy_many)
    with pytest.raises(PermissionError) as caught:
        dummy_fs.copy(
            ["/directory", "/ok", "/retry"],
            ["/dir-copy", "/ok-copy", "/parents/retry"],
            symlinks=True,
        )
    assert caught.value is denied
    assert denied.index == 2
    assert (denied.status_domain, denied.status_code) == ("nfs", 13)
    assert calls == [
        [("/ok", "/ok-copy"), ("/retry", "/parents/retry")],
        [("/retry", "/parents/retry")],
    ]


def test_copy_error_rebases_after_special_source_filter(
    dummy_fs, tmp_path, monkeypatch
):
    os.mkfifo(tmp_path / "root" / "fifo")
    dummy_fs.pipe_file("/ok", b"data")
    denied = PermissionError(errno.EACCES, "server policy")
    denied.index = 0
    calls = []

    def copy_many(pairs):
        calls.append(pairs)
        return [None], {0: denied}

    monkeypatch.setattr(dummy_fs._client, "copy_many", copy_many)
    with pytest.raises(PermissionError) as caught:
        dummy_fs.copy(["/fifo", "/ok"], ["/fifo-copy", "/ok-copy"])
    assert caught.value is denied
    assert denied.index == 1
    assert calls == [[("/ok", "/ok-copy")]]


@pytest.mark.parametrize("duplicate", [False, True])
def test_group_read_error_uses_openfiles_position(dummy_fs, monkeypatch, duplicate):
    paths = ["/first", "/first" if duplicate else "/second"]
    dummy_fs.pipe({path: b"data" for path in paths})
    error = PermissionError(errno.EACCES, "injected read failure")
    error.index = 0

    def fail_current(fds, offsets, lengths):
        return [error] + [b""] * (len(fds) - 1)

    with OpenFiles([OpenFile(dummy_fs, path) for path in paths], fs=dummy_fs) as files:
        monkeypatch.setattr(dummy_fs._client, "preadv", fail_current)
        monkeypatch.setattr(
            dummy_fs,
            "info",
            lambda *args, **kwargs: pytest.fail(
                "error attribution must not fetch metadata"
            ),
        )
        with pytest.raises(PermissionError) as caught:
            files[1].read(1)
        assert caught.value is error
        assert error.index == 1
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("operation", ["rm_file", "rm", "rm_many"])
@pytest.mark.parametrize("failure", ["denied", "missing", "unreported"])
def test_nonrecursive_remove_requires_successful_metadata(
    dummy_fs, monkeypatch, operation, failure
):
    dummy_fs.mkdir("/blocked")
    dummy_fs.pipe_file("/safe", b"keep")
    paths = ["/safe", "/blocked"] if operation == "rm_many" else ["/blocked"]
    index = len(paths) - 1
    cause = None
    if failure != "unreported":
        cause = OSError(
            errno.EACCES if failure == "denied" else errno.ENOENT, "metadata failure"
        )
        cause.index = 99
        cause.status_domain = "nfs"
        cause.status_code = 13 if failure == "denied" else 2
    original = dummy_fs._client.lstat_many
    removed = []

    def metadata(native_paths):
        stats, errors = original(native_paths)
        blocked = dummy_fs._native_path("/blocked")
        if blocked in native_paths:
            position = native_paths.index(blocked)
            stats[position] = None
            if cause is not None:
                errors[position] = cause
        return stats, errors

    with monkeypatch.context() as patch:
        patch.setattr(dummy_fs._client, "lstat_many", metadata)
        remove = dummy_fs._client.remove_many

        def remove_many(native_paths):
            removed.append(native_paths)
            return remove(native_paths)

        patch.setattr(dummy_fs._client, "remove_many", remove_many)
        with pytest.raises(OSError) as caught:
            if operation == "rm_many":
                dummy_fs.rm(paths)
            else:
                getattr(dummy_fs, operation)(paths[0])
        assert removed == (
            [[dummy_fs._native_path("/safe")]] if operation == "rm_many" else []
        )
        if cause is not None:
            assert caught.value is cause
            assert cause.index == index
            assert cause.filename == "/blocked"
            assert cause.status_domain == "nfs"
        else:
            assert caught.value.errno == errno.EIO
            assert caught.value.index == index
    assert dummy_fs.isdir("/blocked")
    if operation == "rm_many":
        assert not dummy_fs.exists("/safe")
    else:
        assert dummy_fs.cat_file("/safe") == b"keep"


@pytest.mark.parametrize("directory_first", [False, True])
@pytest.mark.parametrize("batch_size", [1, 16])
def test_get_error_index_survives_filtering_and_batching(
    dummy_fs, tmp_path, directory_first, batch_size
):
    dummy_fs.batch_size = batch_size
    dummy_fs.pipe_file("/ok", b"keep")
    sources = ["/ok", "/missing"]
    if directory_first:
        dummy_fs.mkdir("/directory")
        sources.insert(0, "/directory")
    targets = [str(tmp_path / f"download-{i}") for i in range(len(sources))]
    with pytest.raises(FileNotFoundError) as caught:
        dummy_fs.get(sources, targets)
    assert caught.value.index == len(sources) - 1
    assert caught.value.filename == "/missing"
    assert (tmp_path / f"download-{int(directory_first)}").read_bytes() == b"keep"
    assert not os.path.exists(targets[-1])


@pytest.mark.parametrize("explicit_options", [False, True])
def test_streamed_get_attributes_disappeared_source_without_creating_parents(
    dummy_fs, tmp_path, monkeypatch, explicit_options
):
    dummy_fs.max_batch_bytes = 8
    dummy_fs.pipe({"/small": b"ok", "/large": b"0123456789"})
    original = dummy_fs.open

    def open_after_disappearance(path, *args, **kwargs):
        if path == "/large":
            dummy_fs.rm_file(path)
        return original(path, *args, **kwargs)

    monkeypatch.setattr(dummy_fs, "open", open_after_disappearance)
    small = tmp_path / "small"
    large = tmp_path / "absent" / "large"
    options = {"cache_type": "none"} if explicit_options else {}
    with pytest.raises(FileNotFoundError) as caught:
        dummy_fs.get(["/small", "/large"], [str(small), str(large)], **options)
    assert getattr(caught.value, "index", None) == 1
    assert caught.value.filename == "/large"
    assert small.read_bytes() == b"ok"
    assert not large.parent.exists()


@pytest.mark.parametrize("operation", ["pipe", "put", "put_with_directory"])
def test_write_transfer_error_index_survives_filtering_and_batching(
    dummy_fs, tmp_path, operation
):
    dummy_fs.batch_size = 1
    with pytest.raises(FileNotFoundError) as caught:
        if operation == "pipe":
            expected = 1
            dummy_fs.pipe({"/ok": b"keep", "/missing/file": b"no"})
        else:
            first = tmp_path / "first"
            second = tmp_path / "second"
            first.write_bytes(b"keep")
            second.write_bytes(b"no")
            sources = [str(first), str(second)]
            targets = ["/ok", "/missing/file"]
            if operation == "put_with_directory":
                directory = tmp_path / "directory"
                directory.mkdir()
                sources.insert(0, str(directory))
                targets.insert(0, "/directory")
            expected = len(sources) - 1
            dummy_fs.put(sources, targets)
    assert caught.value.index == expected
    assert caught.value.filename == "/missing/file"
    assert dummy_fs.cat_file("/ok") == b"keep"
    assert not dummy_fs.exists("/missing/file")


def test_get_rebases_raised_transport_error_without_replaying_downloads(
    dummy_fs, tmp_path, monkeypatch
):
    dummy_fs.batch_size = 1
    dummy_fs.mkdir("/directory")
    dummy_fs.pipe({"/ok": b"keep", "/later": b"not downloaded"})
    targets = [str(tmp_path / f"target-{i}") for i in range(3)]
    (tmp_path / "target-2").write_bytes(b"old local data")
    cause = TimeoutError(errno.ETIMEDOUT, "deadline")
    cause.index = 0
    cause.transport_kind = "timeout"
    original = dummy_fs._client.read_all_many
    calls = []

    def read_many(paths, budget):
        calls.append(list(paths))
        if paths == [dummy_fs._native_path("/later")]:
            raise cause
        return original(paths, budget)

    monkeypatch.setattr(dummy_fs._client, "read_all_many", read_many)
    with pytest.raises(TimeoutError) as caught:
        dummy_fs.get(["/directory", "/ok", "/later"], targets)
    assert caught.value is cause
    assert cause.index == 2
    assert cause.filename == "/later"
    assert cause.transport_kind == "timeout"
    assert len(calls) == 2
    assert (tmp_path / "target-1").read_bytes() == b"keep"
    assert (tmp_path / "target-2").read_bytes() == b"old local data"


@pytest.mark.parametrize("metadata_first", [False, True])
def test_remove_combines_sparse_metadata_and_native_errors_in_public_input_order(
    dummy_fs, monkeypatch, metadata_first
):
    paths = (
        ["/unclassified", "/remove-denied"]
        if metadata_first
        else ["/remove-denied", "/unclassified"]
    )
    dummy_fs.pipe(dict.fromkeys(paths, b"keep"))
    metadata_error = PermissionError(errno.EACCES, "metadata denied")
    mutation_error = PermissionError(errno.EPERM, "remove denied")
    mutation_error.index = 0
    original = dummy_fs._client.lstat_many

    def metadata(native):
        stats, errors = original(native)
        index = native.index(dummy_fs._native_path("/unclassified"))
        stats[index] = None
        errors[index] = metadata_error
        return stats, errors

    calls = []

    def remove(native):
        calls.append(list(native))
        raise mutation_error

    with monkeypatch.context() as patch:
        patch.setattr(dummy_fs._client, "lstat_many", metadata)
        patch.setattr(dummy_fs._client, "remove_many", remove)
        with pytest.raises(PermissionError) as caught:
            dummy_fs.rm(paths)
    expected = metadata_error if metadata_first else mutation_error
    assert caught.value is expected
    assert caught.value.index == 0
    assert calls == [[dummy_fs._native_path("/remove-denied")]]
    assert all(dummy_fs.cat_file(path) == b"keep" for path in paths)
