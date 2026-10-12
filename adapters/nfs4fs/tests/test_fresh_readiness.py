"""Regression coverage for the October 11 production readiness review."""

import errno
import gzip
import shutil

import pytest
from fsspec.core import OpenFile, OpenFiles


@pytest.fixture(params=["dummy_fs", "nfs_fs"])
def fs(request):
    return request.getfixturevalue(request.param)


@pytest.mark.parametrize("operation", ["scalar", "group", "pipe", "put"])
@pytest.mark.parametrize("commit", [False, True])
def test_transaction_write_entry_points_defer_all_sizes(
    fs, tmp_path, operation, commit
):
    fs.max_batch_bytes = 8
    fs.transfer_chunk_size = 4
    fs.auto_mkdir = True
    values = [b"x" * size for size in (7, 8, 13)]
    paths = ["/existing", "/new", "/missing/child"]
    fs.pipe_file(paths[0], b"original")
    transaction = fs.transaction
    transaction.__enter__()
    try:
        if operation == "group":
            with OpenFiles(
                [OpenFile(fs, p, "wb") for p in paths], mode="wb", fs=fs
            ) as files:
                for file, value in zip(files, values):
                    file.write(value)
        elif operation == "pipe":
            fs.pipe(dict(zip(paths, values)))
        elif operation == "put":
            local = [tmp_path / f"source-{i}" for i in range(3)]
            for path, value in zip(local, values):
                path.write_bytes(value)
            fs.put([str(p) for p in local], paths)
        else:
            for path, value in zip(paths, values):
                with fs.open(path, "wb") as file:
                    file.write(value)
        assert fs.cat_file(paths[0]) == b"original"
        assert not fs.exists("/new")
        assert not fs.exists("/missing")
    finally:
        transaction.complete(commit=commit)
    if commit:
        assert [fs.cat_file(path) for path in paths] == values
    else:
        assert fs.cat_file(paths[0]) == b"original"
        assert not fs.exists("/new")
        assert not fs.exists("/missing")
    assert not fs._client._fds


@pytest.mark.parametrize("mode", ["wb", "ab", "xb"])
@pytest.mark.parametrize("compression", [None, "gzip"])
def test_transaction_group_wrappers_finalize_before_publication(fs, mode, compression):
    with fs.transaction:
        entries = [
            OpenFile(fs, "/wrapped", mode.replace("b", "t"), compression=compression)
        ]
        with OpenFiles(entries, mode=mode, fs=fs) as files:
            files[0].write("hello\n")
        assert not fs.exists("/wrapped")
    data = fs.cat_file("/wrapped")
    assert (gzip.decompress(data) if compression else data) == b"hello\n"


@pytest.mark.parametrize("mode", ["rb+", "wb+", "ab+", "xb+"])
@pytest.mark.parametrize("grouped", [False, True])
def test_transaction_update_modes_rejected_without_mutation(fs, mode, grouped):
    fs.pipe_file("/existing", b"original")
    with fs.transaction:
        with pytest.raises(NotImplementedError, match="update"):
            if grouped:
                fs.open_many([OpenFile(fs, "/existing", mode)])
            else:
                fs.open("/existing", mode)
    assert fs.cat_file("/existing") == b"original"
    assert not fs._client._fds


@pytest.mark.parametrize("permissions", [0o600, 0o640, None])
def test_transaction_stage_is_private_and_preserves_destination_mode(
    fs, monkeypatch, permissions
):
    if permissions is not None:
        fs.pipe_file("/secret", b"old")
        fs._client.chmod(fs._native_path("/secret"), permissions)
    write = fs._client.pwrite
    observed = []

    def check_private(fd, data, offset):
        observed.append(fs._client.fstat(fd)["mode"] & 0o777)
        assert observed[-1] == 0o600
        return write(fd, data, offset)

    monkeypatch.setattr(fs._client, "pwrite", check_private)
    with fs.transaction:
        with fs.open("/secret", "wb") as file:
            file.write(b"private contents")
    assert observed
    assert fs.info("/secret")["mode"] & 0o777 == (
        0o600 if permissions is None else permissions
    )
    assert fs.cat_file("/secret") == b"private contents"
    assert not any(".nfs4fs-txn-" in p for p in fs.ls("/", detail=False))


def test_transaction_metadata_failure_preserves_original_and_cleans_stage(
    fs, monkeypatch
):
    fs.pipe_file("/secret", b"old")
    fs._client.chmod(fs._native_path("/secret"), 0o640)

    def fail(*args):
        raise PermissionError(errno.EACCES, "injected metadata failure")

    monkeypatch.setattr(fs._client, "chmod", fail)
    with pytest.raises(PermissionError, match="metadata failure"):
        with fs.transaction:
            with fs.open("/secret", "wb") as file:
                file.write(b"new")
    assert fs.cat_file("/secret") == b"old"
    assert fs.info("/secret")["mode"] & 0o777 == 0o640
    assert not any(".nfs4fs-txn-" in p for p in fs.ls("/", detail=False))
    assert not fs._client._fds


@pytest.mark.parametrize("size", [7, 8, 13])
def test_put_invalid_mode_has_no_side_effects(fs, tmp_path, size):
    fs.max_batch_bytes = 8
    fs.auto_mkdir = True
    fs.pipe_file("/existing", b"original")
    source = tmp_path / "source"
    source.write_bytes(b"x" * size)
    with pytest.raises(ValueError, match="mode"):
        fs.put(str(source), "/existing", mode="typo")
    assert fs.cat_file("/existing") == b"original"
    directory = tmp_path / "tree"
    directory.mkdir()
    (directory / "child").write_bytes(b"x" * size)
    with pytest.raises(ValueError, match="mode"):
        fs.put(str(directory), "/new-directory", recursive=True, mode="typo")
    assert not fs.exists("/new-directory")


@pytest.mark.parametrize("cache", ["none", "readahead"])
@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize("data", [b"x" * 7 + b"\n", b"x" * 8 + b"\n", b"x" * 32])
@pytest.mark.parametrize("method", ["readline", "readuntil", "__next__"])
def test_line_reads_obey_aggregate_budget(fs, cache, grouped, data, method):
    fs.read_all_max_total_bytes = 8
    fs.block_size = 4
    fs.cache_type = cache
    fs.pipe_file("/line", data)
    context = (
        OpenFiles([OpenFile(fs, "/line", "rb")], mode="rb", fs=fs)
        if grouped
        else fs.open("/line", "rb")
    )
    with context as opened:
        file = opened[0] if grouped else opened
        if len(data) > 8:
            with pytest.raises(OSError) as error:
                getattr(file, method)()
            assert error.value.errno == errno.EFBIG
        else:
            assert getattr(file, method)() == data


@pytest.mark.parametrize("cache", ["none", "readahead"])
def test_readuntil_cross_block_delimiter_and_nonzero_offset(fs, cache):
    fs.read_all_max_total_bytes = 8
    fs.block_size = 4
    fs.pipe_file("/line", b"skipabcENDtail")
    with fs.open("/line", "rb", cache_type=cache) as file:
        file.seek(4)
        assert file.readuntil(b"END") == b"abcEND"
        assert file.tell() == 10
        assert file.read(4) == b"tail"


@pytest.mark.parametrize("cache", ["none", "readahead"])
@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize("method", ["readline", "readuntil", "__next__"])
@pytest.mark.parametrize("data,offset", [(b"", 0), (b"line\n", 0), (b"line\n", 5)])
def test_zero_budget_line_reads_distinguish_eof(
    fs, cache, grouped, method, data, offset
):
    fs.pipe_file("/zero-budget", data)
    fs.read_all_max_total_bytes = 0
    fs.cache_type = cache
    context = (
        OpenFiles([OpenFile(fs, "/zero-budget", "rb")], mode="rb", fs=fs)
        if grouped
        else fs.open("/zero-budget", "rb")
    )
    with context as opened:
        file = opened[0] if grouped else opened
        file.seek(offset)
        if offset < len(data):
            with pytest.raises(OSError) as error:
                getattr(file, method)()
            assert error.value.errno == errno.EFBIG
        elif method == "__next__":
            with pytest.raises(StopIteration):
                next(file)
        else:
            assert getattr(file, method)() == b""
        assert file.tell() == offset


def test_zero_budget_uncached_line_probe_observes_growth(fs):
    fs.pipe_file("/growing-line", b"")
    fs.read_all_max_total_bytes = 0
    with fs.open("/growing-line", "rb", cache_type="none") as file:
        fs.pipe_file("/growing-line", b"grown\n")
        with pytest.raises(OSError) as error:
            file.readline()
        assert error.value.errno == errno.EFBIG
        assert file.tell() == 0


@pytest.mark.parametrize("alias", ["same", "hardlink", "symlink", "directory"])
def test_oversized_copy_alias_never_modifies_source(fs, alias):
    fs.mkdir("/real")
    payload = b"a" * (16 * 1024 * 1024) + b"end-marker"
    fs.pipe_file("/real/source", payload)
    if alias == "same":
        dest = "/real/source"
    elif alias == "directory":
        fs.symlink("real", "/alias")
        dest = "/alias/source"
    else:
        dest = "/alias"
        if alias == "hardlink":
            fs.hardlink("/real/source", dest)
        else:
            fs.symlink("real/source", dest)
    with pytest.raises(shutil.SameFileError):
        fs.cp_file("/real/source", dest)
    assert fs.size("/real/source") == len(payload)
    with fs.open("/real/source", "rb", cache_type="none") as source:
        for start in range(0, len(payload), 1024 * 1024):
            assert source.read(1024 * 1024) == payload[start : start + 1024 * 1024]


@pytest.mark.parametrize("commit", [False, True])
def test_transaction_append_preserves_existing_data(fs, commit):
    fs.pipe_file("/append", b"original")
    transaction = fs.transaction
    transaction.__enter__()
    with OpenFiles([OpenFile(fs, "/append", "ab")], mode="ab", fs=fs) as files:
        files[0].write(b"+new")
    assert fs.cat_file("/append") == b"original"
    transaction.complete(commit=commit)
    assert fs.cat_file("/append") == (b"original+new" if commit else b"original")


def test_transaction_directory_upload_rejected_before_creation(fs, tmp_path):
    source = tmp_path / "tree"
    source.mkdir()
    (source / "child").write_bytes(b"data")
    with fs.transaction:
        with pytest.raises(NotImplementedError, match="directory"):
            fs.put(str(source), "/new-tree", recursive=True)
    assert not fs.exists("/new-tree")


def test_transaction_preparation_interrupt_removes_private_stage(fs, monkeypatch):
    fs.pipe_file("/existing", b"original")

    def interrupted(*args):
        raise KeyboardInterrupt()

    monkeypatch.setattr(fs._client, "pwrite", interrupted)
    with pytest.raises(KeyboardInterrupt):
        with fs.transaction:
            with fs.open("/existing", "wb") as file:
                file.write(b"replacement")
    assert fs.cat_file("/existing") == b"original"
    assert not any(".nfs4fs-txn-" in p for p in fs.ls("/", detail=False))
    assert not fs._client._fds


def test_transaction_file_list_retains_vector_publication(fs, tmp_path, monkeypatch):
    sources = [tmp_path / f"file-{i}" for i in range(3)]
    for source in sources:
        source.write_bytes(b"value")
    batches = []
    rename = fs._client.rename_many

    def record(pairs):
        batches.append(len(pairs))
        return rename(pairs)

    monkeypatch.setattr(fs._client, "rename_many", record)
    with fs.transaction:
        fs.put([str(source) for source in sources], ["/a", "/b", "/c"])
    assert batches == [3]


def test_transaction_permissions_do_not_trust_cached_metadata(fs, monkeypatch):
    fs.pipe_file("/secret", b"old")
    fs._client.chmod(fs._native_path("/secret"), 0o600)
    with fs.transaction:
        with fs.open("/secret", "wb") as file:
            file.write(b"new")
        with monkeypatch.context() as patch:
            patch.setattr(fs, "info", lambda path, **kwargs: {"mode": 0o644})
            file.prepare()
    assert fs.info("/secret")["mode"] & 0o777 == 0o600


@pytest.mark.parametrize("failed_index", [0, 2])
def test_transaction_group_setup_failure_closes_failed_and_previous_entries(
    fs, failed_index
):
    fs.pipe_file("/reader", b"readable")
    entries = [
        OpenFile(fs, "/reader", "rb"),
        OpenFile(fs, "/writer", "wb"),
        OpenFile(fs, "/reader", "rb"),
    ]
    entries[failed_index] = OpenFile(fs, "/reader", "rt", encoding="invalid-codec")
    try:
        with pytest.raises(LookupError, match="invalid-codec"):
            with fs.transaction:
                fs.open_many(entries)
        assert not fs._client._fds
        assert all(not entry.fobjects for entry in entries)
        assert not fs.exists("/writer")
    finally:
        for entry in entries:
            entry.close()


@pytest.mark.parametrize("signature", ["legacy", "uninspectable"])
@pytest.mark.parametrize(
    "operation", ["manual", "append", "scalar", "group", "pipe", "put"]
)
def test_transaction_rejects_old_native_before_spooling(
    tmp_path, monkeypatch, signature, operation
):
    from types import SimpleNamespace

    from nfs4fs import Nfs4FileSystem, _native
    from vfsi_fsspec import _fs as engine

    class LegacyClient:
        def __init__(self, *args, **kwargs):
            self.native = _native.NfsClient(*args, **kwargs)

        def __getattr__(self, name):
            return getattr(self.native, name)

        def open(self, path, mode):
            return self.native.open(path, mode)

        def read_all_many(self, paths, max_total_bytes=None):
            if max_total_bytes is None:
                return self.native.read_all_many(paths)
            return self.native.read_all_many(paths, max_total_bytes=max_total_bytes)

    if signature == "uninspectable":
        LegacyClient.open.__signature__ = object()

    class LegacyFs(Nfs4FileSystem):
        _native_module = SimpleNamespace(NfsClient=LegacyClient)

    source = tmp_path / "source"
    source.write_bytes(b"new")
    with LegacyFs(
        backend="dummy",
        dummy_root=str(tmp_path / "remote"),
        skip_instance_cache=True,
        auto_mkdir=True,
    ) as target:
        # Ordinary I/O remains usable with an older native adapter.
        target.pipe_file("/existing", b"original")
        with target.open("/existing", "rb") as reader:
            assert reader.read() == b"original"
        spool = engine.tempfile.SpooledTemporaryFile
        allocations = []

        def record_spool(*args, **kwargs):
            allocations.append(True)
            return spool(*args, **kwargs)

        monkeypatch.setattr(engine.tempfile, "SpooledTemporaryFile", record_spool)
        with target.transaction:
            with pytest.raises(ImportError, match="upgrade"):
                if operation in ("manual", "append"):
                    target.open(
                        "/existing",
                        "ab" if operation == "append" else "wb",
                        autocommit=False,
                    )
                elif operation == "scalar":
                    target.open("/missing/file", "wb")
                elif operation == "group":
                    target.open_many([OpenFile(target, "/missing/file", "wb")])
                elif operation == "pipe":
                    target.pipe_file("/missing/file", b"new")
                else:
                    target.put(str(source), "/missing/file")
            assert not target.transaction.files
        assert allocations == []
        assert target.cat_file("/existing") == b"original"
        assert not target.exists("/missing")
        assert not target._client._fds


@pytest.mark.parametrize("withdirs", [False, True])
def test_du_maxdepth_excludes_deeper_files(dummy_fs, withdirs):
    root = "/du-maxdepth"
    dummy_fs.mkdir(f"{root}/child", create_parents=True)
    dummy_fs.pipe_file(f"{root}/root.txt", b"r")
    dummy_fs.pipe_file(f"{root}/child/deeper.txt", b"d")
    expected = {"nfs4:///du-maxdepth/root.txt": 1}
    if withdirs:
        expected["nfs4:///du-maxdepth"] = dummy_fs.info(root)["size"]
        expected["nfs4:///du-maxdepth/child"] = dummy_fs.info(f"{root}/child")["size"]

    assert dummy_fs.du(root, total=False, withdirs=withdirs, maxdepth=1) == expected


def test_walk_error_handler_reaches_unreadable_subdirectory(dummy_fs, monkeypatch):
    root = "/walk-on-error"
    locked = f"{root}/top/locked"
    readable = f"{root}/top/readable"
    dummy_fs.mkdir(locked, create_parents=True)
    dummy_fs.mkdir(readable, create_parents=True)

    def fail_bulk_walk(internal, fill_epoch, maxdepth=None):
        raise PermissionError(f"no access to {locked}")

    monkeypatch.setattr(dummy_fs, "_native_walk_tree", fail_bulk_walk)
    original_ls = dummy_fs.ls

    def ls(path, detail=True, refresh=False, **kwargs):
        if dummy_fs._checked_strip_protocol(path) == locked:
            raise PermissionError(f"no access to {locked}")
        return original_ls(path, detail=detail, refresh=refresh, **kwargs)

    monkeypatch.setattr(dummy_fs, "ls", ls)
    expected = [
        (root, ["top"], []),
        (f"{root}/top", ["locked", "readable"], []),
        (readable, [], []),
    ]

    assert list(dummy_fs.walk(root, refresh=True)) == expected
    with pytest.raises(PermissionError, match=f"no access to {locked}"):
        list(dummy_fs.walk(root, on_error="raise", refresh=True))

    handled = []
    assert list(dummy_fs.walk(root, on_error=handled.append, refresh=True)) == expected
    assert [str(error) for error in handled] == [f"no access to {locked}"]


def test_walk_fallback_keeps_hard_depth_limit(tmp_path, monkeypatch):
    import fsspec

    fs = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "walk-hard-depth"),
        walk_max_depth=1,
        skip_instance_cache=True,
    )
    fs.mkdir("/one/two", create_parents=True)
    fs.pipe_file("/one/two/file", b"x")

    def fail_bulk_walk(internal, fill_epoch, maxdepth=None):
        raise PermissionError("injected nested traversal failure")

    monkeypatch.setattr(fs, "_native_walk_tree", fail_bulk_walk)
    try:
        with pytest.raises(OSError) as exc_info:
            list(fs.walk("/"))
        assert exc_info.value.errno == errno.EFBIG
    finally:
        fs.close()


def test_recursive_rm_respects_maxdepth(dummy_fs):
    root = "/rm-maxdepth"
    dummy_fs.mkdir(f"{root}/child/deep", create_parents=True)
    dummy_fs.pipe(
        {
            f"{root}/root.txt": b"root",
            f"{root}/child/child.txt": b"child",
            f"{root}/child/deep/deep.txt": b"deep",
        }
    )

    dummy_fs.rm(root, recursive=True, maxdepth=1)

    assert not dummy_fs.exists(f"{root}/root.txt")
    assert dummy_fs.exists(f"{root}/child/child.txt")
    assert dummy_fs.exists(f"{root}/child/deep/deep.txt")


def test_recursive_rm_rejects_zero_maxdepth_without_mutation(dummy_fs):
    dummy_fs.pipe_file("/rm-invalid-depth", b"keep")

    with pytest.raises(ValueError, match="maxdepth"):
        dummy_fs.rm("/rm-invalid-depth", recursive=True, maxdepth=0)

    assert dummy_fs.cat_file("/rm-invalid-depth") == b"keep"


def test_recursive_rm_maxdepth_removes_emptied_root(dummy_fs):
    root = "/rm-maxdepth-empty-root"
    dummy_fs.mkdir(root, create_parents=True)
    dummy_fs.pipe_file(f"{root}/root.txt", b"root")

    dummy_fs.rm(root, recursive=True, maxdepth=1)

    assert not dummy_fs.exists(root)


def test_recursive_rm_maxdepth_removes_empty_shallow_child(dummy_fs):
    root = "/rm-maxdepth-empty-child"
    empty_child = f"{root}/empty"
    deep_file = f"{root}/keep/deep/file.txt"
    dummy_fs.mkdir(empty_child, create_parents=True)
    dummy_fs.mkdir(f"{root}/keep/deep", create_parents=True)
    dummy_fs.pipe_file(deep_file, b"keep")

    dummy_fs.rm(root, recursive=True, maxdepth=1)

    assert not dummy_fs.exists(empty_child)
    assert dummy_fs.exists(root)
    assert dummy_fs.cat_file(deep_file) == b"keep"


def test_recursive_rm_maxdepth_rejects_depth_beyond_walk_limit(tmp_path):
    import fsspec

    fs = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "rm-hard-depth"),
        walk_max_depth=1,
        skip_instance_cache=True,
    )
    root = "/rm-hard-depth"
    fs.mkdir(f"{root}/one/two", create_parents=True)
    fs.pipe(
        {
            f"{root}/root.txt": b"root",
            f"{root}/one/one.txt": b"one",
            f"{root}/one/two/two.txt": b"two",
        }
    )

    try:
        with pytest.raises(OSError) as exc_info:
            fs.rm(root, recursive=True, maxdepth=3)
        assert exc_info.value.errno == errno.EFBIG
        assert fs.exists(f"{root}/root.txt")
        assert fs.exists(f"{root}/one/one.txt")
        assert fs.exists(f"{root}/one/two/two.txt")
    finally:
        fs.close()
