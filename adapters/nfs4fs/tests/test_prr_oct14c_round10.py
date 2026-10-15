"""Bulk byte and local transfer APIs honor declared PathLike/Sequence inputs."""

from pathlib import PurePosixPath

import pytest
from fsspec.callbacks import Callback


class DeclaredPath:
    def __init__(self, path):
        self.path = path

    def __fspath__(self):
        return self.path

    def __str__(self):
        return "/wrong-display-path"


PATH_TYPES = [str, PurePosixPath, DeclaredPath]


@pytest.mark.parametrize("path_type", PATH_TYPES)
def test_cat_scalar_pathlike_returns_bytes(dummy_fs, path_type):
    dummy_fs.pipe_file("/source", b"payload")
    dummy_fs.pipe_file("/wrong-display-path", b"wrong")
    callback = Callback()
    result = dummy_fs.cat(path_type("/source"), callback=callback)
    assert result == b"payload"
    assert callback.size == callback.value == 7


@pytest.mark.parametrize("path_type", PATH_TYPES)
def test_pipe_scalar_pathlike_writes_declared_path(dummy_fs, path_type):
    dummy_fs.pipe_file("/wrong-display-path", b"keep")
    dummy_fs.pipe(path_type("/target"), b"payload")
    assert dummy_fs.cat_file("/target") == b"payload"
    assert dummy_fs.cat_file("/wrong-display-path") == b"keep"


@pytest.mark.parametrize("source_type", PATH_TYPES)
@pytest.mark.parametrize("destination_type", PATH_TYPES)
@pytest.mark.parametrize("download", [False, True])
def test_bulk_transfer_scalar_pathlike_preserves_file_destination(
    dummy_fs, tmp_path, source_type, destination_type, download
):
    local = tmp_path / "local"
    callback = Callback()
    if download:
        dummy_fs.pipe_file("/remote", b"payload")
        dummy_fs.get(
            source_type("/remote"), destination_type(str(local)), callback=callback
        )
        assert local.read_bytes() == b"payload"
    else:
        local.write_bytes(b"payload")
        dummy_fs.put(
            source_type(str(local)), destination_type("/remote"), callback=callback
        )
        assert dummy_fs.cat_file("/remote") == b"payload"
    assert callback.size == callback.value == 1
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("source_container", [list, tuple])
@pytest.mark.parametrize("destination_container", [list, tuple])
@pytest.mark.parametrize("download", [False, True])
@pytest.mark.parametrize("duplicate", [False, True])
def test_bulk_transfer_preserves_paired_sequence_order(
    dummy_fs, tmp_path, source_container, destination_container, download, duplicate
):
    a, b = tmp_path / "a", tmp_path / "b"
    x, y = tmp_path / "x", tmp_path / "y"
    callback = Callback()
    if download:
        dummy_fs.pipe({"/a": b"AAA", "/b": b"BBB"})
        dummy_fs.get(
            source_container(["/b", "/b" if duplicate else "/a"]),
            destination_container([str(x), str(y)]),
            callback=callback,
        )
        assert x.read_bytes() == b"BBB"
        assert y.read_bytes() == (b"BBB" if duplicate else b"AAA")
    else:
        a.write_bytes(b"AAA")
        b.write_bytes(b"BBB")
        dummy_fs.put(
            source_container([str(b), str(b) if duplicate else str(a)]),
            destination_container(["/x", "/y"]),
            callback=callback,
        )
        assert dummy_fs.cat_file("/x") == b"BBB"
        assert dummy_fs.cat_file("/y") == (b"BBB" if duplicate else b"AAA")
    assert callback.size == callback.value == 2


@pytest.mark.parametrize("download", [False, True])
def test_bulk_transfer_tuple_length_mismatch_has_no_side_effects(
    dummy_fs, tmp_path, download
):
    local = tmp_path / "local"
    local.write_bytes(b"original")
    dummy_fs.pipe_file("/remote", b"remote original")
    with pytest.raises(ValueError, match="equal length"):
        if download:
            dummy_fs.get(("/remote",), (str(local), str(tmp_path / "extra")))
        else:
            dummy_fs.put((str(local),), ("/remote", "/extra"))
    assert local.read_bytes() == b"original"
    assert dummy_fs.cat_file("/remote") == b"remote original"
    assert not dummy_fs.exists("/extra")


@pytest.mark.parametrize("download", [False, True])
@pytest.mark.parametrize("source_slash", [False, True])
def test_bulk_transfer_custom_pathlike_keeps_directory_separator(
    dummy_fs, tmp_path, download, source_slash
):
    source = tmp_path / "source"
    source.mkdir()
    (source / "file").write_bytes(b"payload")
    dummy_fs.mkdir("/destination")
    local_destination = tmp_path / "destination"
    local_destination.mkdir()
    if download:
        dummy_fs.makedirs("/source")
        dummy_fs.pipe_file("/source/file", b"payload")
        dummy_fs.get(
            DeclaredPath("/source" + ("/" if source_slash else "")),
            DeclaredPath(str(local_destination) + "/"),
            recursive=True,
        )
        expected = local_destination if source_slash else local_destination / "source"
        assert (expected / "file").read_bytes() == b"payload"
    else:
        dummy_fs.put(
            DeclaredPath(str(source) + ("/" if source_slash else "")),
            DeclaredPath("/destination/"),
            recursive=True,
        )
        expected = "/destination" if source_slash else "/destination/source"
        assert dummy_fs.cat_file(expected + "/file") == b"payload"
