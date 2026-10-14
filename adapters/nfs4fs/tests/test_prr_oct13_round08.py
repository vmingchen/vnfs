"""PRR round 8: scalar wrapper setup failures must not publish or leak."""

import errno
import weakref

import pytest
from vfsi_fsspec import _fs

BAD_OPTIONS = [
    {"encoding": "prr-invalid-codec"},
    {"newline": "invalid-newline"},
    {"compression": "invalid-compression"},
]


@pytest.mark.parametrize(
    "mode,options",
    [("rt", option) for option in BAD_OPTIONS] + [("rb", BAD_OPTIONS[2])],
)
def test_scalar_wrapper_failure_closes_native_handle(dummy_fs, options, mode):
    dummy_fs.pipe_file("/source", b"original")
    with pytest.raises((LookupError, ValueError)) as error:
        dummy_fs.open("/source", mode, **options)
    assert error.value is not None  # Retain the full failed setup traceback.
    assert not dummy_fs._client._fds
    assert dummy_fs.cat_file("/source") == b"original"


@pytest.mark.parametrize("options", BAD_OPTIONS)
@pytest.mark.parametrize("mode", ["wt", "at", "xt"])
def test_caught_wrapper_failure_is_not_committed(dummy_fs, options, mode):
    if mode != "xt":
        dummy_fs.pipe_file("/target", b"original")
    with dummy_fs.transaction as transaction:
        with pytest.raises((LookupError, ValueError)):
            dummy_fs.open("/target", mode, **options)
        assert not transaction.files
        with dummy_fs.open("/good", "wt", encoding="utf8") as file:
            file.write("valid")
    if mode == "xt":
        assert not dummy_fs.exists("/target")
    else:
        assert dummy_fs.cat_file("/target") == b"original"
    assert dummy_fs.cat_file("/good") == b"valid"


@pytest.mark.parametrize("mode", ["rb", "wb", "ab", "xb"])
@pytest.mark.parametrize("close_fails", [False, True])
def test_compressor_constructor_failure_retains_primary_and_cleanup(
    dummy_fs, monkeypatch, mode, close_fails
):
    if mode != "xb":
        dummy_fs.pipe_file("/target", b"original")
    primary = RuntimeError("compression constructor failed")
    cleanup = OSError(errno.EIO, "native CLOSE failed")
    references = []

    def fail_compressor(file, **kwargs):
        references.append(weakref.ref(file))
        raise primary

    def fail_close(*args):
        raise cleanup

    with monkeypatch.context() as patch:
        patch.setitem(_fs.compr, "gzip", fail_compressor)
        if close_fails:
            patch.setattr(dummy_fs._client, "close", fail_close)
        with pytest.raises(RuntimeError) as error:
            dummy_fs.open("/target", mode, compression="gzip")
        assert error.value is primary
        if close_fails:
            assert error.value.__cause__ is cleanup
            assert set(dummy_fs._client._fds) <= dummy_fs._client._deferred_close
        else:
            assert not dummy_fs._client._fds
            assert references[0]().closed
    dummy_fs.info("/target")
    assert not dummy_fs._client._fds


def test_failed_transaction_wrapper_discards_spool(dummy_fs, monkeypatch):
    dummy_fs.pipe_file("/target", b"original")
    references = []
    factory = _fs.tempfile.SpooledTemporaryFile

    def capture(*args, **kwargs):
        spool = factory(*args, **kwargs)
        references.append(weakref.ref(spool))
        return spool

    monkeypatch.setattr(_fs.tempfile, "SpooledTemporaryFile", capture)
    with dummy_fs.transaction as transaction:
        with pytest.raises(LookupError) as error:
            dummy_fs.open("/target", "wt", encoding="prr-invalid-codec")
        assert error.value is not None
        assert references[0]() is None or references[0]().closed
        assert not transaction.files
    assert dummy_fs.cat_file("/target") == b"original"


@pytest.mark.parametrize("transactional", [False, True])
def test_successful_text_wrapper_keeps_normal_ownership(dummy_fs, transactional):
    def write():
        with dummy_fs.open("/target", "wt", encoding="utf-16-le") as file:
            file.write("hello")

    if transactional:
        with dummy_fs.transaction:
            write()
    else:
        write()
    with dummy_fs.open("/target", "rt", encoding="utf-16-le") as file:
        assert file.read() == "hello"
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("options", BAD_OPTIONS[:2])
def test_text_setup_error_survives_native_close_failure(dummy_fs, monkeypatch, options):
    dummy_fs.pipe_file("/source", b"original")
    cleanup = OSError(errno.EIO, "native CLOSE failed")

    def fail_close(*args):
        raise cleanup

    with monkeypatch.context() as patch:
        patch.setattr(dummy_fs._client, "close", fail_close)
        with pytest.raises((LookupError, ValueError)) as error:
            dummy_fs.open("/source", "rt", **options)
        assert error.value.__cause__ is cleanup
        assert len(dummy_fs._client._fds) == 1
        assert set(dummy_fs._client._fds) <= dummy_fs._client._deferred_close
    dummy_fs.info("/source")
    assert not dummy_fs._client._fds


def test_failed_transaction_setup_preserves_error_if_discard_fails(
    dummy_fs, monkeypatch
):
    dummy_fs.pipe_file("/target", b"original")
    discard = _fs._DeferredWriteFile.discard
    cleanup = OSError(errno.EIO, "discard cleanup failed")

    def fail_after_discard(file):
        discard(file)
        raise cleanup

    with dummy_fs.transaction as transaction:
        with monkeypatch.context() as patch:
            patch.setattr(_fs._DeferredWriteFile, "discard", fail_after_discard)
            with pytest.raises(LookupError) as error:
                dummy_fs.open("/target", "wt", encoding="prr-invalid-codec")
            assert error.value.__cause__ is cleanup
            assert not transaction.files
    assert dummy_fs.cat_file("/target") == b"original"
