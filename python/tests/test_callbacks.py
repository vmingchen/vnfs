"""Progress callback coverage for vectorized fsspec transfers."""

import pytest
from fsspec.callbacks import Callback


class RecordingCallback(Callback):
    """Capture parent and branched-child state without relying on tqdm."""

    def __init__(self, root=None, paths=None):
        super().__init__()
        self.root = self if root is None else root
        self.paths = paths
        self.events = []
        if root is None:
            self.children = []

    def call(self, **kwargs):
        self.events.append((self.size, self.value))

    def branched(self, path_1, path_2, **kwargs):
        child = type(self)(root=self.root, paths=(path_1, path_2))
        self.root.children.append(child)
        return child


def _children(callback):
    return {child.paths: child for child in callback.children}


def _assert_complete(callback, size):
    assert callback.size == size
    assert callback.value == size
    assert callback.events[-1] == (size, size)


def test_cat_callbacks_cover_batched_streamed_ranges_and_errors(dummy_fs):
    dummy_fs.max_batch_bytes = 5
    dummy_fs.transfer_chunk_size = 2
    dummy_fs.pipe({"/small": b"abc", "/large": b"1234567"})

    callback = RecordingCallback()
    assert dummy_fs.cat(["/small", "/large"], callback=callback) == {
        "/small": b"abc",
        "/large": b"1234567",
    }
    _assert_complete(callback, 2)
    children = _children(callback)
    _assert_complete(children[("/small", "<memory>")], 3)
    _assert_complete(children[("/large", "<memory>")], 7)
    assert [value for _, value in children[("/large", "<memory>")].events] == [
        0,
        2,
        4,
        6,
        7,
    ]

    direct = RecordingCallback()
    assert dummy_fs.cat_file("/large", start=1, end=5, callback=direct) == b"2345"
    _assert_complete(direct, 4)
    assert direct.children == []

    ranges = RecordingCallback()
    result = dummy_fs.cat_ranges(
        ["/small", "/missing", "/large"],
        [0, 0, 2],
        [2, 1, 5],
        callback=ranges,
    )
    assert result[0] == b"ab"
    assert isinstance(result[1], FileNotFoundError)
    assert result[2] == b"345"
    _assert_complete(ranges, 3)
    range_children = _children(ranges)
    _assert_complete(range_children[("/small", "<memory>")], 2)
    _assert_complete(range_children[("/large", "<memory>")], 3)
    assert ("/missing", "<memory>") not in range_children


def test_pipe_callbacks_cover_bulk_direct_streaming_and_create(dummy_fs):
    dummy_fs.max_batch_bytes = 5
    dummy_fs.transfer_chunk_size = 2

    callback = RecordingCallback()
    dummy_fs.pipe({"/small": b"abc", "/large": b"1234567"}, callback=callback)
    _assert_complete(callback, 2)
    children = _children(callback)
    _assert_complete(children[("<memory>", "/small")], 3)
    _assert_complete(children[("<memory>", "/large")], 7)
    assert [value for _, value in children[("<memory>", "/large")].events] == [
        0,
        2,
        4,
        6,
        7,
    ]

    direct = RecordingCallback()
    dummy_fs.pipe_file("/exclusive", b"abcdef", mode="create", callback=direct)
    _assert_complete(direct, 6)
    assert [value for _, value in direct.events] == [0, 2, 4, 6]


def test_put_and_get_callbacks_finish_parent_and_report_file_bytes(dummy_fs, tmp_path):
    dummy_fs.max_batch_bytes = 5
    dummy_fs.transfer_chunk_size = 2
    local_a = tmp_path / "a.bin"
    local_b = tmp_path / "b.bin"
    local_a.write_bytes(b"abc")
    local_b.write_bytes(b"1234567")

    put_callback = RecordingCallback()
    dummy_fs.put(
        [str(local_a), str(local_b)],
        ["/a.bin", "/b.bin"],
        callback=put_callback,
    )
    _assert_complete(put_callback, 2)
    put_children = _children(put_callback)
    _assert_complete(put_children[(str(local_a), "/a.bin")], 3)
    _assert_complete(put_children[(str(local_b), "/b.bin")], 7)

    out_a = tmp_path / "out-a.bin"
    out_b = tmp_path / "out-b.bin"
    get_callback = RecordingCallback()
    dummy_fs.get(
        ["/a.bin", "/b.bin"],
        [str(out_a), str(out_b)],
        callback=get_callback,
    )
    _assert_complete(get_callback, 2)
    get_children = _children(get_callback)
    _assert_complete(get_children[("/a.bin", str(out_a))], 3)
    _assert_complete(get_children[("/b.bin", str(out_b))], 7)
    assert out_a.read_bytes() == b"abc"
    assert out_b.read_bytes() == b"1234567"


def test_copy_callbacks_cover_single_list_glob_and_recursive(dummy_fs):
    dummy_fs.mkdir("/src")
    dummy_fs.pipe({"/src/a.bin": b"abc", "/src/b.bin": b"12345"})

    direct = RecordingCallback()
    dummy_fs.cp_file("/src/a.bin", "/direct.bin", callback=direct)
    _assert_complete(direct, 3)

    dummy_fs.mkdir("/list")
    listed = RecordingCallback()
    dummy_fs.copy(
        ["/src/a.bin", "/src/b.bin"],
        ["/list/a.bin", "/list/b.bin"],
        callback=listed,
    )
    _assert_complete(listed, 2)
    listed_children = _children(listed)
    _assert_complete(listed_children[("/src/a.bin", "/list/a.bin")], 3)
    _assert_complete(listed_children[("/src/b.bin", "/list/b.bin")], 5)

    dummy_fs.mkdir("/glob")
    globbed = RecordingCallback()
    dummy_fs.copy("/src/*.bin", "/glob/", callback=globbed)
    _assert_complete(globbed, 2)
    assert dummy_fs.cat_file("/glob/a.bin") == b"abc"
    assert dummy_fs.cat_file("/glob/b.bin") == b"12345"

    recursive = RecordingCallback()
    dummy_fs.copy("/src", "/tree", recursive=True, callback=recursive)
    _assert_complete(recursive, 2)
    assert dummy_fs.cat_file("/tree/a.bin") == b"abc"
    assert dummy_fs.cat_file("/tree/b.bin") == b"12345"


def test_none_callback_is_accepted_for_all_transfer_entry_points(dummy_fs, tmp_path):
    source = tmp_path / "source"
    target = tmp_path / "target"
    source.write_bytes(b"data")

    dummy_fs.pipe_file("/a", b"data", callback=None)
    dummy_fs.pipe({"/b": b"data"}, callback=None)
    assert dummy_fs.cat_file("/a", callback=None) == b"data"
    assert dummy_fs.cat(["/a", "/b"], callback=None) == {
        "/a": b"data",
        "/b": b"data",
    }
    assert dummy_fs.cat_ranges(["/a"], 0, 2, callback=None) == [b"da"]
    dummy_fs.put(str(source), "/put", callback=None)
    dummy_fs.get("/put", str(target), callback=None)
    dummy_fs.cp_file("/a", "/cp-file", callback=None)
    dummy_fs.copy(["/a"], ["/copy"], callback=None)
    assert target.read_bytes() == b"data"


@pytest.mark.parametrize("fs_fixture", ["dummy_fs", "nfs_fs", "smb_fs"])
def test_callback_smoke_on_every_backend(request, fs_fixture, tmp_path):
    fs = request.getfixturevalue(fs_fixture)
    fs.mkdir("/callback")

    pipe_callback = RecordingCallback()
    fs.pipe({"/callback/a": b"abc", "/callback/b": b"defg"}, callback=pipe_callback)
    _assert_complete(pipe_callback, 2)

    cat_callback = RecordingCallback()
    assert fs.cat(["/callback/a", "/callback/b"], callback=cat_callback) == {
        "/callback/a": b"abc",
        "/callback/b": b"defg",
    }
    _assert_complete(cat_callback, 2)

    copy_callback = RecordingCallback()
    fs.copy(
        ["/callback/a", "/callback/b"],
        ["/callback/c", "/callback/d"],
        callback=copy_callback,
    )
    _assert_complete(copy_callback, 2)

    source = tmp_path / f"{fs_fixture}-source"
    target = tmp_path / f"{fs_fixture}-target"
    source.write_bytes(b"transfer")
    put_callback = RecordingCallback()
    fs.put(str(source), "/callback/upload", callback=put_callback)
    _assert_complete(put_callback, 1)
    get_callback = RecordingCallback()
    fs.get("/callback/upload", str(target), callback=get_callback)
    _assert_complete(get_callback, 1)
    assert target.read_bytes() == b"transfer"
