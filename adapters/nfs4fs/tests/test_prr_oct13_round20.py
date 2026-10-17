"""Persistent cache wrappers share direct-open cleanup and read budgets."""

import bz2
import errno
import gzip
import lzma

import fsspec
import pytest
from vfsi_fsspec import _fs

CAP = 1 << 20
CODECS = {"gzip": gzip.compress, "bz2": bz2.compress, "xz": lzma.compress}


def cached_fs(fs, tmp_path):
    return fsspec.filesystem(
        "blockcache",
        fs=fs,
        cache_storage=str(tmp_path / "cache"),
        skip_instance_cache=True,
    )


@pytest.mark.parametrize("codec", CODECS)
@pytest.mark.parametrize("text", [False, True])
def test_cached_compressed_close_releases_retained_owner(
    dummy_fs, tmp_path, codec, text
):
    dummy_fs.pipe_file("/data", CODECS[codec](b"hello\n"))
    cached = cached_fs(dummy_fs, tmp_path)
    file = cached.open("/data", "rt" if text else "rb", compression=codec)
    assert len(dummy_fs._client._fds) == 1
    assert file.read() == ("hello\n" if text else b"hello\n")
    file.close()
    assert file.closed
    assert not dummy_fs._client._fds
    file.close()
    with cached.open("/data", "rb", compression=codec) as complete:
        assert not dummy_fs._client._fds  # Complete encoded generation is local.
        assert complete.read() == b"hello\n"
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("codec", CODECS)
def test_cached_codec_writer_finalizes_and_releases_owner(dummy_fs, tmp_path, codec):
    cached = cached_fs(dummy_fs, tmp_path)
    file = cached.open("/data", "wb", compression=codec)
    file.write(b"valid")
    file.close()
    assert not dummy_fs._client._fds
    with dummy_fs.open("/data", "rb", compression=codec) as reader:
        assert reader.read() == b"valid"


@pytest.mark.parametrize("codec", CODECS)
@pytest.mark.parametrize("complete", [False, True])
@pytest.mark.parametrize("method", ["read", "readline", "next"])
def test_cached_codec_budget_applies_to_remote_and_complete_hits(
    dummy_fs, tmp_path, codec, complete, method
):
    payload = b"x" * (CAP + 20)
    dummy_fs.read_all_max_total_bytes = CAP
    encoded_payload = CODECS[codec](payload)
    dummy_fs.pipe_file("/data", encoded_payload)
    cached = cached_fs(dummy_fs, tmp_path)
    if complete:
        with cached.open("/data", "rb") as encoded:
            assert encoded.read() == encoded_payload
    with cached.open("/data", "rb", compression=codec) as file:
        with pytest.raises(OSError) as error:
            next(file) if method == "next" else getattr(file, method)()
        assert error.value.errno == errno.EFBIG
        assert error.value.filename == "/data"
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("compression", [None, "gzip"])
def test_cached_text_setup_failure_closes_owner_with_retained_traceback(
    dummy_fs, tmp_path, compression
):
    dummy_fs.pipe_file("/data", gzip.compress(b"hello") if compression else b"hello")
    cached = cached_fs(dummy_fs, tmp_path)
    with pytest.raises(LookupError) as error:
        cached.open(
            "/data", "rt", compression=compression, encoding="prr-invalid-codec"
        )
    assert error.value is not None
    assert not dummy_fs._client._fds


def test_cached_codec_constructor_failure_cleans_owner(dummy_fs, tmp_path, monkeypatch):
    dummy_fs.pipe_file("/data", b"hello")
    cached = cached_fs(dummy_fs, tmp_path)
    owners = []
    primary = RuntimeError("codec setup failed")

    def fail(file, **kwargs):
        owners.append(file)
        raise primary

    monkeypatch.setitem(_fs.compr, "gzip", fail)
    with pytest.raises(RuntimeError) as error:
        cached.open("/data", "rb", compression="gzip")
    assert error.value is primary
    assert owners[0].closed
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("body_fails", [False, True])
def test_cached_codec_close_failure_is_deferred_and_preserves_body_error(
    dummy_fs, tmp_path, monkeypatch, body_fails
):
    dummy_fs.pipe_file("/data", gzip.compress(b"hello"))
    cached = cached_fs(dummy_fs, tmp_path)
    primary = RuntimeError("application failed")
    cleanup = OSError(errno.EIO, "native CLOSE failed")

    def fail(*args):
        raise cleanup

    with monkeypatch.context() as patch:
        patch.setattr(dummy_fs._client, "close", fail)
        with pytest.raises(RuntimeError if body_fails else OSError) as error:
            with cached.open("/data", "rb", compression="gzip") as file:
                assert file.read(1) == b"h"
                if body_fails:
                    raise primary
        assert error.value is (primary if body_fails else cleanup)
        if body_fails:
            assert error.value.__cause__ is cleanup
        assert set(dummy_fs._client._fds) <= dummy_fs._client._deferred_close
    dummy_fs.info("/data")
    assert not dummy_fs._client._fds


def test_cached_non_vfsi_backend_keeps_original_open(tmp_path):
    cached = fsspec.filesystem(
        "blockcache",
        fs=fsspec.filesystem("file"),
        cache_storage=str(tmp_path / "other"),
        skip_instance_cache=True,
    )
    path = str(tmp_path / "prr20.gz")
    with cached.open(path, "wb", compression="gzip") as file:
        assert isinstance(file, gzip.GzipFile)
        owner = file.fileobj
        file.write(b"other backend")
    # fsspec retains its existing owner behavior for unrelated backends.
    owner.close()
    with gzip.open(path, "rb") as file:
        assert file.read() == b"other backend"


@pytest.mark.parametrize(
    "options",
    [
        {"encoding": "prr-invalid-codec"},
        {"compression": "invalid-compression"},
    ],
)
def test_caught_cached_transaction_setup_failure_is_not_published(
    dummy_fs, tmp_path, options
):
    dummy_fs.pipe_file("/target", b"original")
    cached = cached_fs(dummy_fs, tmp_path)
    with cached.transaction as transaction:
        with pytest.raises((LookupError, ValueError)) as error:
            cached.open("/target", "wt", **options)
        assert error.value is not None
        assert not transaction.files
        with cached.open("/good.gz", "wt", compression="gzip") as file:
            file.write("valid")
    assert dummy_fs.cat_file("/target") == b"original"
    with dummy_fs.open("/good.gz", "rt", compression="gzip") as file:
        assert file.read() == "valid"
    assert not dummy_fs._client._fds


def test_cached_inferred_compression_and_positional_options(dummy_fs, tmp_path):
    dummy_fs.pipe_file("/data.gz", gzip.compress(b"hello"))
    cached = cached_fs(dummy_fs, tmp_path)
    with cached.open("/data.gz", "rb", 64, {}, "infer") as file:
        assert file.read() == b"hello"
    assert not dummy_fs._client._fds
