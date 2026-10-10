"""Python file and fsspec contracts on local and live NFS backends."""

import bz2
import errno
import gzip
import os

import pytest
from fsspec.core import OpenFile, OpenFiles


@pytest.fixture(params=["dummy_fs", "nfs_fs"])
def fs(request):
    return request.getfixturevalue(request.param)


@pytest.mark.parametrize("buffered", [False, True])
@pytest.mark.parametrize("compression", [None, "gzip", "bz2"])
@pytest.mark.parametrize("text", [False, True])
def test_grouped_wrappers_preserve_formats_and_batch_opens(
    fs, monkeypatch, compression, text, buffered
):
    fs.write_buffering = buffered
    paths = ["/first", "/second"]
    value = "caf\u00e9\n" if text else b"caf\xc3\xa9\n"
    expected = value.encode("utf-16-le") if text else value
    original = fs._client.open_many
    batches = []

    def record(paths, modes):
        batches.append((paths, modes))
        return original(paths, modes)

    monkeypatch.setattr(fs._client, "open_many", record)
    write_mode = "wt" if text else "wb"
    entries = [
        OpenFile(
            fs,
            path,
            mode=write_mode,
            compression=compression,
            encoding="utf-16-le",
            newline="",
        )
        for path in paths
    ]
    with OpenFiles(entries, mode=write_mode, fs=fs) as writers:
        for writer in writers:
            assert writer.write(value) == len(value)
    assert all(writer.closed for writer in writers)
    assert len(batches) == 1
    decode = {None: lambda data: data, "gzip": gzip.decompress, "bz2": bz2.decompress}[
        compression
    ]
    for path in paths:
        assert decode(fs.cat_file(path)) == expected
    read_mode = "rt" if text else "rb"
    entries = [
        OpenFile(
            fs,
            path,
            mode=read_mode,
            compression=compression,
            encoding="utf-16-le",
            newline="",
        )
        for path in paths
    ]
    with OpenFiles(entries, mode=read_mode, fs=fs) as readers:
        assert [reader.read() for reader in readers] == [value, value]
    assert len(batches) == 2
    assert all(reader.closed for reader in readers)
    assert not fs._client._fds


def test_group_wrapper_setup_failure_closes_every_native_handle(fs):
    fs.pipe({"/first": b"a", "/second": b"b"})
    entries = [
        OpenFile(fs, "/first", mode="rt"),
        OpenFile(fs, "/second", mode="rt", encoding="invalid-codec"),
    ]
    with pytest.raises(LookupError):
        fs.open_many(entries)
    assert not fs._client._fds


def test_direct_commit_many_finishes_compression_wrappers(fs):
    entries = [OpenFile(fs, "/compressed", mode="wt", compression="gzip")]
    files = fs.open_many(entries)
    files[0].write("payload")
    fs.commit_many(files)
    assert gzip.decompress(fs.cat_file("/compressed")) == b"payload"
    assert files[0].closed
    assert not fs._client._fds


@pytest.mark.parametrize("kind", ["file", "symlink", "directory_symlink"])
def test_rmdir_rejects_non_directories_without_removing_them(fs, kind):
    fs.pipe_file("/target", b"keep")
    if kind == "file":
        path = "/target"
    else:
        if kind == "directory_symlink":
            fs.mkdir("/directory")
        fs.symlink("directory" if kind == "directory_symlink" else "target", "/link")
        path = "/link"
    with pytest.raises(NotADirectoryError) as error:
        fs.rmdir(path)
    assert error.value.errno == errno.ENOTDIR
    assert fs.exists(path)
    assert fs.cat_file("/target") == b"keep"
    fs.mkdir("/empty")
    fs.rmdir("/empty")
    assert not fs.exists("/empty")


@pytest.mark.parametrize("manual", [False, True])
def test_deferred_append_writes_at_end_after_seek(fs, manual):
    fs.pipe_file("/append", b"abc")
    if manual:
        file = fs.open("/append", "ab", autocommit=False)
        file.seek(0)
        file.write(b"Z")
        file.seek(1)
        file.write(b"Y")
        file.close()
        file.commit()
    else:
        with fs.transaction:
            with fs.open("/append", "ab") as file:
                file.seek(0)
                file.write(b"Z")
                file.seek(1)
                file.write(b"Y")
    assert fs.cat_file("/append") == b"abcZY"


@pytest.mark.parametrize("mode", ["rb", "r+b"])
def test_uncached_handle_observes_external_growth_and_truncation(fs, mode):
    fs.pipe_file("/data", b"abc")
    with fs.open("/data", mode, cache_type="none") as file:
        with fs.open("/data", "ab") as other:
            other.write(b"def")
        assert file.read() == b"abcdef"
        assert file.seek(-4, os.SEEK_END) == 2
        with fs.open("/data", "r+b") as other:
            other.truncate(1)
        assert file.seek(0, os.SEEK_END) == 1
        file.seek(0)
        assert file.read() == b"a"


def test_update_handle_seek_end_uses_its_current_size(fs):
    fs.pipe_file("/data", b"abc")
    with fs.open("/data", "r+b") as file:
        file.write(b"abcdef")
        assert file.size == 6
        assert file.seek(-4, os.SEEK_END) == 2
        assert file.read() == b"cdef"
        file.truncate(1)
        assert file.size == 1
        with pytest.raises(OSError):
            file.seek(-2, os.SEEK_END)


@pytest.mark.parametrize("mode", ["wb", "ab", "xb"])
@pytest.mark.parametrize("commit", [False, True])
def test_transaction_auto_mkdir_is_deferred_until_commit(fs, mode, commit):
    fs.auto_mkdir = True
    transaction = fs.transaction
    transaction.__enter__()
    with fs.open("/missing/parent/file", mode) as file:
        file.write(b"payload")
    assert not fs.exists("/missing")
    transaction.complete(commit=commit)
    if commit:
        assert fs.cat_file("/missing/parent/file") == b"payload"
    else:
        assert not fs.exists("/missing")


@pytest.mark.parametrize("commit", [False, True])
def test_transaction_cleanup_attempts_all_files_and_preserves_primary_error(
    fs, monkeypatch, commit
):
    transaction = fs.transaction
    transaction.__enter__()
    files = []
    for path in ["/first", "/second"]:
        with fs.open(path, "xb") as file:
            file.write(b"payload")
        file.prepare()
        files.append(file)
    primary = OSError(errno.EIO, "publication failure")
    cleanup = OSError(errno.EACCES, "cleanup failure")
    remove = fs.rm
    removed = []

    def fail_publish(*args):
        raise primary

    def remove_except_first(path, *args, **kwargs):
        removed.append(path)
        if path == files[0].temp_path:
            raise cleanup
        return remove(path, *args, **kwargs)

    try:
        with monkeypatch.context() as patch:
            patch.setattr(fs._client, "hardlink", fail_publish)
            patch.setattr(fs, "rm", remove_except_first)
            with pytest.raises(OSError) as error:
                transaction.complete(commit=commit)
        assert error.value is (primary if commit else cleanup)
        if commit:
            assert error.value.__cause__ is cleanup
        assert removed == [file.temp_path for file in files]
        assert all(file._spool.closed for file in files)
        assert not fs.exists(files[1].temp_path)
        assert fs._transaction is None
        assert not fs._intrans
    finally:
        for file in files:
            file.discard()


@pytest.mark.parametrize("compression", [None, "gzip", "bz2"])
def test_wrapped_buffered_exit_uses_vector_writes(fs, monkeypatch, compression):
    fs.write_buffering = True
    original = fs._client.pwrite_many
    waves = []

    def record(fds, offsets, data):
        waves.append(len(fds))
        return original(fds, offsets, data)

    monkeypatch.setattr(fs._client, "pwrite_many", record)
    entries = [
        OpenFile(
            fs, f"/batch-{i}", mode="wt", compression=compression if i == 3 else None
        )
        for i in range(4)
    ]
    with OpenFiles(entries, mode="wt", fs=fs) as files:
        for file in files:
            file.write("payload")
    assert waves == [4]
    assert not fs._client._fds


@pytest.mark.parametrize("buffered", [False, True])
def test_wrapper_write_failure_closes_every_handle_without_replay(
    fs, monkeypatch, buffered
):
    fs.write_buffering = buffered
    entries = [OpenFile(fs, path, mode="wt") for path in ["/a", "/b"]]
    context = OpenFiles(entries, mode="wt", fs=fs)
    calls = []
    failure = OSError(errno.ENOSPC, "injected full server")

    def fail(*args):
        calls.append(args)
        raise failure

    with monkeypatch.context() as patch:
        patch.setattr(fs._client, "pwrite_many" if buffered else "pwrite", fail)
        with pytest.raises(OSError) as error:
            with context as files:
                for file in files:
                    file.write("payload")
        assert error.value is failure
        count = len(calls)
        with pytest.raises(OSError) as error:
            fs.commit_many(files)
        assert error.value is failure
        assert len(calls) == count
    assert not fs._client._fds
    assert all(file._write_spool is None for file in files.raw_files)
    assert all("__exit__" not in entry.__dict__ for entry in entries)


def test_wrapper_encoder_failure_attempts_siblings_and_never_retries(fs, monkeypatch):
    from vfsi_fsspec._fs import compr

    fs.write_buffering = True
    closed = []
    failure = OSError(errno.EIO, "encoder failure")

    class BrokenEncoder:
        def __init__(self, sink, mode):
            self.sink = sink
            self.closed = False

        def write(self, data):
            return self.sink.write(data)

        def close(self):
            closed.append(self)
            self.sink.write(b"incomplete-trailer")
            # Deliberately remain open after a failure, as some codecs do.
            raise failure

    monkeypatch.setitem(compr, "review-broken-codec", BrokenEncoder)
    entries = [
        OpenFile(fs, path, mode="wb", compression="review-broken-codec")
        for path in ["/a", "/b"]
    ]
    with pytest.raises(OSError) as error:
        with OpenFiles(entries, mode="wb", fs=fs) as files:
            for file in files:
                file.write(b"payload")
    assert error.value is failure
    assert len(closed) == 2
    assert not fs._client._fds
    assert fs.cat_file("/a") == fs.cat_file("/b") == b""
    with pytest.raises(OSError):
        fs.commit_many(files)
    assert len(closed) == 2


@pytest.mark.parametrize("compression", [None, "gzip"])
@pytest.mark.parametrize("write_failure", [False, True])
def test_wrapped_native_close_retry_never_refinalizes_or_rewrites(
    fs, monkeypatch, write_failure, compression
):
    fs.write_buffering = True
    entries = [OpenFile(fs, "/retry", mode="wt", compression=compression)]
    files = fs.open_many(entries)
    files[0].write("payload")
    original_write = fs._client.pwrite_many
    writes = []
    primary = OSError(errno.ENOSPC, "write failure")
    cleanup = OSError(errno.EIO, "close failure")

    def record(*args):
        writes.append(args)
        if write_failure:
            raise primary
        return original_write(*args)

    def fail_close(*args):
        raise cleanup

    with monkeypatch.context() as patch:
        patch.setattr(fs._client, "pwrite_many", record)
        patch.setattr(fs._client, "close_many", fail_close)
        with pytest.raises(OSError) as error:
            fs.commit_many(files)
        assert error.value is (primary if write_failure else cleanup)
        if write_failure:
            assert error.value.__cause__ is cleanup
    assert fs._client._fds
    assert files[0].closed
    with pytest.raises(ValueError):
        files[0].write("must not accept bytes after finalization")
    count = len(writes)
    with monkeypatch.context() as patch:
        patch.setattr(fs._client, "pwrite_many", record)
        if write_failure:
            with pytest.raises(OSError) as error:
                fs.commit_many(files)
            assert error.value is primary
        else:
            fs.commit_many(files)
            fs.commit_many(files)
    assert len(writes) == count
    assert not fs._client._fds
    if not write_failure:
        stored = fs.cat_file("/retry")
        assert (gzip.decompress(stored) if compression else stored) == b"payload"


@pytest.mark.parametrize("via_entry", [False, True])
def test_individual_wrapper_close_and_explicit_flush_remain_immediate(fs, via_entry):
    fs.write_buffering = True
    # Observe through a separate session: HEAD also invalidates a live NFS
    # writer's stateid when a path read reuses that same native session.
    options = {**fs.storage_options, "skip_instance_cache": True}
    with type(fs)(*fs.storage_args, **options) as observer:
        entries = [OpenFile(fs, path, mode="wt") for path in ["/a", "/b"]]
        files = fs.open_many(entries)
        files[0].write("first")
        files[0].flush()
        assert observer.cat_file("/a") == b"first"
        files[0].write(" second")
        (entries[0] if via_entry else files[0]).close()
        assert observer.cat_file("/a") == b"first second"
        assert not files[1].closed
        files[1].write("sibling")
        fs.commit_many(files)
        assert observer.cat_file("/b") == b"sibling"
        assert all(file.closed for file in files)
        assert not fs._client._fds


def test_standalone_openfile_still_owns_its_wrappers(fs):
    entry = OpenFile(fs, "/standalone", mode="wt", compression="gzip")
    with entry as file:
        file.write("payload")
    assert file.closed
    assert gzip.decompress(fs.cat_file("/standalone")) == b"payload"


def test_wrapper_finalization_respects_memory_budget(fs, monkeypatch):
    from vfsi_fsspec._fs import _BufferGroup

    fs.write_buffering = True
    fs.max_batch_bytes = 16
    fs.block_size = 8
    original = _BufferGroup.enforce_memory_limit
    retained = []

    def measure(group):
        original(group)
        retained.append(group.in_memory_bytes())

    monkeypatch.setattr(_BufferGroup, "enforce_memory_limit", measure)
    entries = [
        OpenFile(fs, f"/bounded-{i}", mode="wt", compression="gzip") for i in range(8)
    ]
    with OpenFiles(entries, mode="wt", fs=fs) as files:
        for file in files:
            file.write("payload" * 100)
    assert retained and max(retained) <= fs.max_batch_bytes
    for entry in entries:
        assert gzip.decompress(fs.cat_file(entry.path)) == b"payload" * 100


def test_mixed_append_and_overwrite_wrappers_use_separate_write_waves(fs, monkeypatch):
    fs.write_buffering = True
    fs.pipe({"/append": b"old", "/overwrite": b"old"})
    entries = [
        OpenFile(fs, "/append", mode="at"),
        OpenFile(fs, "/overwrite", mode="wt"),
    ]
    appends, writes = [], []
    append, write = fs._client.append_many, fs._client.pwrite_many

    def record_append(fds, data):
        appends.append(len(fds))
        return append(fds, data)

    def record_write(fds, offsets, data):
        writes.append(len(fds))
        return write(fds, offsets, data)

    with monkeypatch.context() as patch:
        patch.setattr(fs._client, "append_many", record_append)
        patch.setattr(fs._client, "pwrite_many", record_write)
        with OpenFiles(entries, mode="wt", fs=fs) as files:
            for file in files:
                file.write("new")
    assert appends == [1] and writes == [1]
    assert fs.cat_file("/append") == b"oldnew"
    assert fs.cat_file("/overwrite") == b"new"


def test_buffered_whole_read_uses_open_size_without_extra_stat(fs, monkeypatch):
    fs.pipe_file("/data", b"abc")
    original = fs._client.fstat
    calls = []

    def record(fd):
        calls.append(fd)
        return original(fd)

    with monkeypatch.context() as patch:
        patch.setattr(fs._client, "fstat", record)
        with fs.open("/data", "rb") as file:
            with fs.open("/data", "ab") as writer:
                writer.write(b"def")
            calls.clear()
            assert file.read() == b"abc"
            assert file.size == file.seek(0, os.SEEK_END) == 3
            assert not calls


def test_buffered_whole_read_handles_shrink(fs):
    fs.pipe_file("/data", b"abcdef")
    with fs.open("/data", "rb") as file:
        with fs.open("/data", "r+b") as writer:
            writer.truncate(2)
        assert file.read() == b"ab"
        assert file.tell() == 2
