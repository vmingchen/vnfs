"""Reproductions for behavior divergences found when comparing the nfs4
filesystem with LocalFileSystem / the fsspec contract.

Each test runs against both backends (dummy + NFS, when reachable). They are
written to fail on the pre-fix behavior and pass after the fixes.
"""

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
