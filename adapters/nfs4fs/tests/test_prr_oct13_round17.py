"""Constructor failures retain neither native handles nor half-built writers."""

import errno

import pytest
from fsspec.core import OpenFile
from vfsi_fsspec import _fs


@pytest.mark.parametrize("mode", ["wb", "ab", "xb", "rb", "wb+", "ab+", "xb+", "rb+"])
def test_invalid_cache_options_fail_before_native_mutation(dummy_fs, mode):
    if not mode.startswith("x"):
        dummy_fs.pipe_file("/target", b"original")
    with pytest.raises(TypeError) as error:
        dummy_fs.open("/target", mode, cache_options=1)
    assert error.value is not None
    assert not dummy_fs._client._fds
    if mode.startswith("x"):
        assert not dummy_fs.exists("/target")
    else:
        assert dummy_fs.cat_file("/target") == b"original"


def test_invalid_group_cache_options_fail_before_truncation_or_mkdir(dummy_fs):
    fs = dummy_fs
    fs.auto_mkdir = True
    fs.pipe_file("/target", b"original")
    with pytest.raises(TypeError):
        fs.open_many(
            [OpenFile(fs, path, mode="wb") for path in ["/target", "/new-parent/file"]],
            cache_options=1,
        )
    assert fs.cat_file("/target") == b"original"
    assert not fs.exists("/new-parent")
    assert not fs._client._fds


@pytest.mark.parametrize(
    "options,error",
    [
        ({"cache_type": "unknown"}, ValueError),
        ({"block_size": "bad"}, ValueError),
        ({"cache_type": "readahead", "cache_options": {"typo": True}}, TypeError),
        ({"cache_type": "blockcache", "cache_options": {"maxblocks": "8"}}, TypeError),
        ({"cache_type": "parts", "cache_options": {"data": [1]}}, TypeError),
        ({"cache_type": "parts", "cache_options": {"data": {"bad": b"x"}}}, TypeError),
    ],
)
def test_invalid_group_reader_options_do_not_truncate_writer(dummy_fs, options, error):
    fs = dummy_fs
    fs.pipe({"/reader": b"readable", "/writer": b"original"})
    with pytest.raises(error):
        fs.open_many(
            [OpenFile(fs, "/writer", mode="wb"), OpenFile(fs, "/reader", mode="rb")],
            write_buffering=True,
            **options,
        )
    assert fs.cat_file("/writer") == b"original"
    assert not fs._client._fds


def test_shared_mmap_location_fails_before_grouped_writer_open(dummy_fs, tmp_path):
    fs = dummy_fs
    fs.pipe({"/first": b"first", "/other": b"other", "/writer": b"original"})
    location = tmp_path / "cache"
    with pytest.raises(ValueError, match="one grouped reader"):
        fs.open_many(
            [
                OpenFile(fs, "/writer", mode="wb"),
                OpenFile(fs, "/first", mode="rb"),
                OpenFile(fs, "/other", mode="rb"),
            ],
            cache_type="mmap",
            cache_options={"location": location},
        )
    assert fs.cat_file("/writer") == b"original"
    assert not location.exists()
    assert not fs._client._fds

    files = fs.open_many(
        [OpenFile(fs, "/first", mode="rb"), OpenFile(fs, "/other", mode="rb")],
        cache_type="mmap",
        block_size=2,
    )
    try:
        assert [file.read() for file in files] == [b"first", b"other"]
        assert files[0].seek(0) == 0
        assert files[0].read() == b"first"
    finally:
        fs.commit_many(files)


@pytest.mark.parametrize("inherited", [False, True])
def test_mmap_setup_failure_precedes_grouped_writer_open(dummy_fs, tmp_path, inherited):
    fs = dummy_fs
    fs.pipe({"/reader": b"payload", "/writer": b"original"})
    fs.auto_mkdir = True
    location = tmp_path / "cache-directory"
    location.mkdir()
    options = {"location": location}
    if inherited:
        fs.cache_options = options
        options = None
    with pytest.raises(IsADirectoryError):
        fs.open_many(
            [
                OpenFile(fs, "/writer", mode="wb"),
                OpenFile(fs, "/reader", mode="rb"),
                OpenFile(fs, "/new/file", mode="wb"),
            ],
            cache_type="mmap",
            cache_options=options,
        )
    assert fs.cat_file("/writer") == b"original"
    assert not fs.exists("/new")
    assert not fs._client._fds


@pytest.mark.parametrize("cache_type", ["readahead", "mmap"])
@pytest.mark.parametrize("existing", [False, True])
def test_grouped_lazy_reader_can_open_after_writer(dummy_fs, cache_type, existing):
    fs = dummy_fs
    if existing:
        fs.pipe_file("/target", b"original")
    files = fs.open_many(
        [OpenFile(fs, "/target", mode="wb"), OpenFile(fs, "/target", mode="rb")],
        cache_type=cache_type,
    )
    try:
        assert files[1].read(1) == b""
        files[0].write(b"new")
    finally:
        fs.commit_many(files)
    assert fs.cat_file("/target") == b"new"
    assert not fs._client._fds


@pytest.mark.parametrize("cache_type", ["mmap", "all"])
@pytest.mark.parametrize("mode", ["wb", "ab", "ab+", "xb"])
@pytest.mark.parametrize("symlink", [False, True])
def test_grouped_reader_can_follow_missing_target(dummy_fs, cache_type, mode, symlink):
    fs = dummy_fs
    reader = "/target"
    if symlink:
        fs.symlink("/target", "/alias")
        reader = "/alias"
    files = fs.open_many(
        [OpenFile(fs, "/target", mode=mode), OpenFile(fs, reader, mode="rb")],
        cache_type=cache_type,
    )
    try:
        assert files[1].read(1) == b""
        files[0].write(b"new")
    finally:
        fs.commit_many(files)
    assert fs.cat_file("/target") == b"new"
    assert not fs._client._fds


def test_existing_append_mmap_reader_validates_backing_before_writer(
    dummy_fs, tmp_path
):
    fs = dummy_fs
    fs.pipe({"/append": b"existing", "/writer": b"original"})
    location = tmp_path / "directory"
    location.mkdir()
    with pytest.raises(IsADirectoryError):
        fs.open_many(
            [
                OpenFile(fs, "/append", mode="ab"),
                OpenFile(fs, "/writer", mode="wb"),
                OpenFile(fs, "/append", mode="rb"),
            ],
            cache_type="mmap",
            cache_options={"location": location},
        )
    assert fs.cat_file("/writer") == b"original"
    assert fs.cat_file("/append") == b"existing"
    assert not fs._client._fds


def test_grouped_mmap_reader_refreshes_truncated_hardlink_size(dummy_fs):
    fs = dummy_fs
    fs.pipe_file("/target", b"original")
    fs.hardlink("/target", "/alias")
    files = fs.open_many(
        [OpenFile(fs, "/target", mode="wb"), OpenFile(fs, "/alias", mode="rb")],
        cache_type="mmap",
    )
    try:
        assert files[1].read(1) == b""
    finally:
        fs.commit_many(files)
    assert not fs._client._fds


@pytest.mark.parametrize("sizes", [None, [None, 8, None]])
def test_eager_read_budget_fails_before_grouped_writer_open(dummy_fs, sizes):
    fs = dummy_fs
    fs.pipe({"/reader": b"readable", "/writer": b"keep"})
    fs.read_all_max_total_bytes = 4
    fs.auto_mkdir = True
    requests = [
        OpenFile(fs, "/writer", mode="wb"),
        OpenFile(fs, "/reader", mode="rb"),
        OpenFile(fs, "/new/file", mode="wb"),
    ]
    with pytest.raises(OSError) as raised:
        fs.open_many(requests, cache_type="all", sizes=sizes)
    assert raised.value.errno == errno.EFBIG
    assert raised.value.filename == "/reader"
    assert fs.cat_file("/writer") == b"keep"
    assert not fs.exists("/new")
    assert not fs._client._fds

    files = fs.open_many(
        requests[:2],
        cache_type="all",
        cache_options={"data": b"cached!!"},
        sizes=[None, 8],
    )
    try:
        assert files[1].read(4) == b"cach"
        files[0].write(b"done")
    finally:
        fs.commit_many(files)
    assert fs.cat_file("/writer") == b"done"


def test_mixed_eager_read_failure_reports_original_index(dummy_fs, monkeypatch):
    fs = dummy_fs
    fs.pipe({"/reader": b"payload", "/writer": b"original"})

    def fail_reads(tokens, offsets, lengths):
        return [OSError(errno.EIO, "eager read failed") for _ in tokens]

    with monkeypatch.context() as patch:
        patch.setattr(fs._client, "preadv", fail_reads)
        with pytest.raises(OSError, match="eager read failed") as raised:
            fs.open_many(
                [
                    OpenFile(fs, "/writer", mode="wb"),
                    OpenFile(fs, "/reader", mode="rb"),
                ],
                cache_type="all",
            )
    assert raised.value.index == 1
    assert raised.value.filename == "/reader"
    assert not fs._client._fds
    assert fs.cat_file("/writer") == b"original"


def test_eager_group_closes_restored_reader_after_writer_open_failure(
    dummy_fs, monkeypatch
):
    fs = dummy_fs
    fs.pipe({"/reader": b"payload", "/writer": b"original"})
    client = fs._client
    open_many = client.open_many
    preadv = client.preadv
    tokens_seen = []

    def reconnect_once(tokens, offsets, lengths):
        tokens_seen.extend(tokens)
        if len(tokens_seen) == 1:
            return [ConnectionError("reader connection lost")]
        return preadv(tokens, offsets, lengths)

    def fail_writer(paths, modes):
        if modes == ["wb"]:
            error = PermissionError(errno.EACCES, "writer open failed")
            error.index = 0
            raise error
        return open_many(paths, modes)

    with monkeypatch.context() as patch:
        patch.setattr(client, "preadv", reconnect_once)
        patch.setattr(client, "open_many", fail_writer)
        with pytest.raises(PermissionError, match="writer open failed") as raised:
            fs.open_many(
                [
                    OpenFile(fs, "/reader", mode="rb"),
                    OpenFile(fs, "/writer", mode="wb"),
                ],
                cache_type="all",
            )
    assert raised.value.index == 1
    assert len(set(tokens_seen)) == 2, "the eager reader must have been restored"
    assert not client._fds
    assert fs.cat_file("/writer") == b"original"


def test_mixed_eager_group_pins_reader_snapshot_before_writer_open(dummy_fs):
    fs = dummy_fs
    fs.pipe_file("/target", b"original")
    files = fs.open_many(
        [OpenFile(fs, "/target", mode="wb"), OpenFile(fs, "/target", mode="rb")],
        cache_type="all",
    )
    try:
        files[0].write(b"updated")
        assert files[1].read() == b"original"
    finally:
        fs.commit_many(files)
    assert fs.cat_file("/target") == b"updated"
    assert not fs._client._fds


def test_grouped_parts_cache_uses_an_independent_copy_for_each_reader(
    dummy_fs, monkeypatch
):
    fs = dummy_fs
    fs.pipe({"/first": b"data", "/second": b"data"})
    known = {(0, 2): b"da", (2, 4): b"ta"}
    files = fs.open_many(
        [OpenFile(fs, path, mode="rb") for path in ["/first", "/second"]],
        block_size=2,
        cache_type="parts",
        cache_options={"data": known, "strict": True},
    )
    try:

        def no_backend_read(*args):
            pytest.fail("configured cache parts must satisfy the full read")

        monkeypatch.setattr(fs._client, "preadv", no_backend_read)
        assert [file.read(3) for file in files] == [b"dat", b"dat"]
        assert known == {(0, 2): b"da", (2, 4): b"ta"}
    finally:
        fs.commit_many(files)


def test_write_only_group_does_not_consult_read_cache_type(dummy_fs):
    fs = dummy_fs
    files = fs.open_many([OpenFile(fs, "/target", mode="wb")], cache_type=[])
    try:
        files[0].write(b"replacement")
    finally:
        fs.commit_many(files)
    assert fs.cat_file("/target") == b"replacement"


def test_missing_required_cache_option_does_not_truncate_writer(dummy_fs, monkeypatch):
    fs = dummy_fs
    fs.pipe({"/reader": b"readable", "/writer": b"original"})

    class RequiredOptionCache(_fs.caches["readahead"]):
        def __init__(self, blocksize, fetcher, size, *, token):
            self.token = token
            super().__init__(blocksize, fetcher, size)

    monkeypatch.setitem(_fs.caches, "required-option", RequiredOptionCache)
    with pytest.raises(TypeError, match="token"):
        fs.open_many(
            [OpenFile(fs, "/writer", mode="wb"), OpenFile(fs, "/reader", mode="rb")],
            cache_type="required-option",
        )
    assert fs.cat_file("/writer") == b"original"
    assert not fs._client._fds


@pytest.mark.parametrize("mode", ["wb", "ab", "xb"])
@pytest.mark.parametrize("close_fails", [False, True])
def test_buffer_allocation_failure_cleans_up_raw_owner(
    dummy_fs, monkeypatch, mode, close_fails
):
    if mode != "xb":
        dummy_fs.pipe_file("/target", b"original")
    primary = MemoryError("buffer allocation failed")
    cleanup = OSError(errno.EIO, "CLOSE failed")
    facades = []
    original_init = _fs.AbstractBufferedFile.__init__

    def fail_buffer(file, *args, **kwargs):
        facades.append(file)
        original_init(file, *args, **kwargs)
        raise primary

    def fail_close(*args):
        raise cleanup

    with monkeypatch.context() as patch:
        patch.setattr(_fs.AbstractBufferedFile, "__init__", fail_buffer)
        if close_fails:
            patch.setattr(dummy_fs._client, "close", fail_close)
        with pytest.raises(MemoryError) as error:
            dummy_fs.open("/target", mode, write_buffering=True)
        assert error.value is primary
        assert facades[0].closed
        if close_fails:
            assert error.value.__cause__ is cleanup
            assert set(dummy_fs._client._fds) <= dummy_fs._client._deferred_close
        else:
            assert not dummy_fs._client._fds
            assert facades[0]._raw.closed
        facades[0].close()  # No flush or second finalization of partial setup.
    dummy_fs.info("/target")
    assert not dummy_fs._client._fds


def test_actual_bytes_buffer_allocation_failure_closes_handle(dummy_fs, monkeypatch):
    def fail(*args, **kwargs):
        raise MemoryError("BytesIO allocation failed")

    with monkeypatch.context() as patch:
        patch.setattr(_fs.io, "BytesIO", fail)
        with pytest.raises(MemoryError) as error:
            dummy_fs.open("/target", "wb", write_buffering=True)
        assert error.value is not None
        assert not dummy_fs._client._fds


@pytest.mark.parametrize("buffered", [False, True])
def test_successful_constructor_retains_owner_until_close(dummy_fs, buffered):
    file = dummy_fs.open("/target", "wb", write_buffering=buffered)
    assert not file.closed
    assert len(dummy_fs._client._fds) == 1
    file.write(b"valid")
    file.close()
    assert file.closed and not dummy_fs._client._fds
    assert dummy_fs.cat_file("/target") == b"valid"


@pytest.mark.parametrize("mode", ["rb", "wb", "ab"])
@pytest.mark.parametrize("failure_index", [0, 1])
def test_group_constructor_failure_closes_every_remaining_token(
    dummy_fs, monkeypatch, mode, failure_index
):
    for path in ["/a", "/b", "/c"]:
        dummy_fs.pipe_file(path, b"original")
    original_init = _fs.AbstractBufferedFile.__init__
    facades = []
    primary = MemoryError("group buffer allocation failed")

    def fail_selected(file, *args, **kwargs):
        facades.append(file)
        original_init(file, *args, **kwargs)
        if len(facades) - 1 == failure_index:
            raise primary

    monkeypatch.setattr(_fs.AbstractBufferedFile, "__init__", fail_selected)
    with pytest.raises(MemoryError) as error:
        dummy_fs.open_many(
            [OpenFile(dummy_fs, path, mode) for path in ["/a", "/b", "/c"]],
            write_buffering=True,
        )
    assert error.value is primary
    assert error.value.__cause__ is None
    assert not dummy_fs._client._fds
    assert not dummy_fs._client._deferred_close
    assert facades[-1].closed
