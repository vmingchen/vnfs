"""Normalize filesystem paths before authority checks and backend dispatch."""

import gzip

import fsspec
import pytest


class DifferentStringPath:
    def __init__(self, path):
        self.path = path

    def __fspath__(self):
        return self.path

    def __str__(self):
        return "/different"


def test_pathlike_writes_use_fspath_not_display_string(dummy_fs):
    dummy_fs.pipe_file(DifferentStringPath("/expected"), b"payload")
    assert dummy_fs.cat_file("/expected") == b"payload"
    assert not dummy_fs.exists("/different")


@pytest.mark.parametrize("operation", ["info", "cat_file", "open", "ls"])
def test_pathlike_reads_use_the_declared_filesystem_path(dummy_fs, operation):
    dummy_fs.pipe_file("/expected", b"right")
    dummy_fs.pipe_file("/different", b"wrong")
    path = DifferentStringPath("/expected")
    if operation == "open":
        with dummy_fs.open(path, "rb") as file:
            assert file.read() == b"right"
    elif operation == "cat_file":
        assert dummy_fs.cat_file(path) == b"right"
    elif operation == "info":
        assert dummy_fs.info(path) == dummy_fs.info("/expected")
    else:
        assert dummy_fs.ls(path, detail=False) == dummy_fs.ls("/expected", detail=False)


def test_pathlike_compression_inference_uses_fspath(dummy_fs):
    dummy_fs.pipe_file("/expected.gz", gzip.compress(b"right"))
    dummy_fs.pipe_file("/different", b"wrong")
    with dummy_fs.open(
        DifferentStringPath("/expected.gz"), "rb", compression="infer"
    ) as file:
        assert file.read() == b"right"


@pytest.mark.parametrize("url", ["nfs4://:2049/data", "nfs4://:/data"])
@pytest.mark.parametrize("operation", ["pipe_file", "open", "cat_file", "cached"])
def test_hostname_free_authority_is_rejected_before_access(
    dummy_fs, tmp_path, url, operation
):
    dummy_fs.pipe_file("/data", b"original")
    with pytest.raises(ValueError):
        if operation == "pipe_file":
            dummy_fs.pipe_file(url, b"wrong")
        elif operation == "open":
            dummy_fs.open(url, "wb")
        elif operation == "cat_file":
            dummy_fs.cat_file(url)
        else:
            cached = fsspec.filesystem(
                "blockcache",
                fs=dummy_fs,
                cache_storage=str(tmp_path / "cache"),
                skip_instance_cache=True,
            )
            cached.open(url, "rb")
    assert dummy_fs.cat_file("/data") == b"original"
    assert not dummy_fs._client._fds


def test_pathlike_authority_cannot_bypass_instance_host(dummy_fs):
    dummy_fs.pipe_file("/data", b"original")
    with pytest.raises(ValueError):
        dummy_fs.pipe_file(DifferentStringPath("nfs4://other-server/data"), b"wrong")
    assert dummy_fs.cat_file("/data") == b"original"


def test_standard_pathlike_empty_authority_and_literal_path_controls(dummy_fs):
    from pathlib import PurePosixPath

    dummy_fs.pipe_file(PurePosixPath("/data"), b"valid")
    assert dummy_fs.cat_file("nfs4:///data") == b"valid"
    dummy_fs.pipe_file("/a?b#c", b"literal")
    assert dummy_fs.cat_file("/a?b#c") == b"literal"


@pytest.mark.parametrize("operation", ["glob", "expand_path"])
def test_pathlike_pattern_operations_use_fspath(dummy_fs, operation):
    dummy_fs.pipe_file("/expected", b"right")
    path = DifferentStringPath("/exp*")
    assert getattr(dummy_fs, operation)(path) == getattr(dummy_fs, operation)("/exp*")
