"""Reproductions for behavior divergences found when comparing the nfs4
filesystem with LocalFileSystem / the fsspec contract.

Each test runs against both backends (dummy + NFS, when reachable). They are
written to fail on the pre-fix behavior and pass after the fixes.
"""

import datetime
import io

import pytest


@pytest.fixture(params=["dummy", "nfs"])
def fs(request, dummy_fs, nfs_fs):
    return dummy_fs if request.param == "dummy" else nfs_fs


def test_checksum_changes_when_contents_change(fs):
    # The contract: if the checksum is the same, the contents are the same.
    fs.pipe_file("nfs4:///f.txt", b"v1")
    before = fs.checksum("nfs4:///f.txt")
    fs.pipe_file("nfs4:///f.txt", b"v2-longer")
    after = fs.checksum("nfs4:///f.txt")
    assert isinstance(before, int)
    assert before != after


def test_absolute_symlink_targets_resolve_inside_root(fs):
    # Absolute targets are chroot-relative: they must resolve within the
    # filesystem root, not the export root (NFS) or the OS root (dummy).
    fs.pipe_file("nfs4:///target.txt", b"t")
    fs.symlink("nfs4:///target.txt", "nfs4:///proto-link.txt")
    assert fs.cat_file("nfs4:///proto-link.txt") == b"t"
    fs.symlink("/target.txt", "nfs4:///abs-link.txt")
    assert fs.cat_file("nfs4:///abs-link.txt") == b"t"
    assert fs.info("nfs4:///abs-link.txt")["type"] == "file"


def test_relative_symlink_target_unchanged(fs):
    fs.pipe_file("nfs4:///target.txt", b"t")
    fs.symlink("target.txt", "nfs4:///rel-link.txt")
    assert fs.readlink("nfs4:///rel-link.txt") == "target.txt"
    assert fs.cat_file("nfs4:///rel-link.txt") == b"t"


def test_rm_non_recursive_does_not_remove_directories(fs):
    fs.mkdir("nfs4:///d", create_parents=True)
    with pytest.raises(IsADirectoryError):
        fs.rm("nfs4:///d")
    assert fs.isdir("nfs4:///d")
    # Files are still removable without recursion.
    fs.pipe_file("nfs4:///d/f.txt", b"x")
    fs.rm("nfs4:///d/f.txt")
    assert not fs.exists("nfs4:///d/f.txt")


def test_rm_non_recursive_removes_symlink_to_directory(fs):
    # os.remove() removes the link itself; lstat-based guarding must not
    # refuse it.
    fs.mkdir("nfs4:///real", create_parents=True)
    fs.symlink("real", "nfs4:///lnk")
    fs.rm("nfs4:///lnk")
    assert not fs.exists("nfs4:///lnk")
    assert fs.isdir("nfs4:///real")


def test_write_str_raises_type_error(fs):
    with fs.open("nfs4:///f.txt", "wb") as fh:
        with pytest.raises(TypeError):
            fh.write("text")
    assert fs.cat_file("nfs4:///f.txt") == b""


def test_truncate_requires_writable_handle(fs):
    fs.pipe_file("nfs4:///f.txt", b"0123456789")
    with fs.open("nfs4:///f.txt", "rb") as fh:
        with pytest.raises(io.UnsupportedOperation):
            fh.truncate(3)
    assert fs.size("nfs4:///f.txt") == 10


def test_open_files_read_mode_closes_descriptors(fs):
    from fsspec.core import OpenFile, OpenFiles

    fs.mkdir("nfs4:///d", create_parents=True)
    fs.pipe_file("nfs4:///d/a.txt", b"a")
    fs.pipe_file("nfs4:///d/b.txt", b"b")
    open_files = OpenFiles(
        [
            OpenFile(fs, "nfs4:///d/a.txt", "rb"),
            OpenFile(fs, "nfs4:///d/b.txt", "rb"),
        ],
        mode="rb",
        fs=fs,
    )
    files = open_files.__enter__()
    assert all(f._fd is not None for f in files)
    assert [f.read() for f in files] == [b"a", b"b"]
    open_files.__exit__(None, None, None)
    assert all(f.closed for f in files)
    assert all(f._fd is None for f in files)


def test_ukey_changes_on_rename_like_local(fs):
    fs.pipe_file("nfs4:///a.txt", b"x")
    before = fs.ukey("nfs4:///a.txt")
    fs.mv("nfs4:///a.txt", "nfs4:///b.txt")
    after = fs.ukey("nfs4:///b.txt")
    assert before != after


def test_cat_file_and_ranges_negative_bounds(fs):
    # fsspec's cat_file/cat_ranges contract: negative start/end are offsets
    # backwards from the end, like Python slices.
    fs.pipe_file("nfs4:///f.txt", b"0123456789")
    assert fs.cat_file("nfs4:///f.txt", start=-3) == b"789"
    assert fs.cat_file("nfs4:///f.txt", end=-1) == b"012345678"
    assert fs.cat_file("nfs4:///f.txt", start=-4, end=-1) == b"678"
    ranges = fs.cat_ranges(
        ["nfs4:///f.txt", "nfs4:///f.txt"], [-3, 0], [-1, 4]
    )
    assert ranges == [b"78", b"0123"]


def test_cat_glob_single_match_returns_dict(fs):
    # A glob that expands to a single file is still an expansion: base cat
    # returns {path: data}, not raw bytes.
    fs.pipe_file("nfs4:///only.txt", b"x")
    out = fs.cat("nfs4:///only.*")
    assert isinstance(out, dict)
    assert out == {"/only.txt": b"x"}


def test_mv_file_into_existing_directory(fs):
    # Like LocalFileSystem (shutil.move) and base mv (copy+rm), moving a file
    # onto an existing directory moves it inside.
    fs.mkdir("nfs4:///dest", create_parents=True)
    fs.pipe_file("nfs4:///src.txt", b"x")
    fs.mv("nfs4:///src.txt", "nfs4:///dest")
    assert not fs.exists("nfs4:///src.txt")
    assert fs.cat_file("nfs4:///dest/src.txt") == b"x"


def test_transaction_commits_on_exit(fs):
    # fsspec transaction contract (see test_local.py test_commit_discard):
    # writes are deferred until the transaction exits; on normal exit they
    # are committed.
    with fs.transaction:
        with fs.open("nfs4:///tx.txt", "wb") as fh:
            fh.write(b"tx")
        assert not fs.exists("nfs4:///tx.txt")
    assert fs._transaction is None
    assert fs.cat_file("nfs4:///tx.txt") == b"tx"


def test_transaction_discards_on_error(fs):
    with pytest.raises(RuntimeError):
        with fs.transaction:
            with fs.open("nfs4:///tx2.txt", "wb") as fh:
                fh.write(b"tx2")
            raise RuntimeError("boom")
    assert fs._transaction is None
    assert not fs.exists("nfs4:///tx2.txt")


def test_created_modified_are_utc_aware(fs):
    # LocalFileSystem returns tz-aware UTC datetimes.
    fs.pipe_file("nfs4:///f.txt", b"x")
    created = fs.created("nfs4:///f.txt")
    modified = fs.modified("nfs4:///f.txt")
    assert created.tzinfo is not None and created.utcoffset() == datetime.timedelta(0)
    assert modified.tzinfo is not None and modified.utcoffset() == datetime.timedelta(0)
