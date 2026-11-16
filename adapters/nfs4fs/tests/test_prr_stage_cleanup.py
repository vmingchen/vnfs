"""Owned staging names remain tracked through failed preparation cleanup."""

import errno

import pytest


@pytest.mark.parametrize("failure_at", ["upload", "chmod"])
def test_transaction_retries_owned_stage_cleanup_after_preparation_failure(
    dummy_fs, monkeypatch, failure_at
):
    dummy_fs.pipe_file("/destination", b"original")
    mutation_error = OSError(errno.ENOSPC, "injected preparation failure")
    cleanup_error = OSError(errno.EACCES, "injected staging unlink failure")
    remove = dummy_fs.rm
    unlink_attempts = []

    def fail_mutation(*args, **kwargs):
        raise mutation_error

    def fail_first_unlink(path, *args, **kwargs):
        unlink_attempts.append(path)
        if len(unlink_attempts) == 1:
            raise cleanup_error
        return remove(path, *args, **kwargs)

    with monkeypatch.context() as patch:
        patch.setattr(
            dummy_fs._client,
            "pwrite" if failure_at == "upload" else "chmod",
            fail_mutation,
        )
        patch.setattr(dummy_fs, "rm", fail_first_unlink)
        with pytest.raises(OSError) as error:
            with dummy_fs.transaction:
                with dummy_fs.open("/destination", "wb") as file:
                    file.write(b"replacement")
        assert error.value is mutation_error
    assert unlink_attempts == [file.temp_path, file.temp_path]
    assert not dummy_fs.exists(file.temp_path)
    assert dummy_fs.cat_file("/destination") == b"original"
    assert file._spool.closed
    assert not dummy_fs._client._fds


def test_failed_manual_preparation_retains_cleanup_ownership(dummy_fs, monkeypatch):
    file = dummy_fs.open("/destination", "wb", autocommit=False)
    file.write(b"replacement")
    mutation_error = OSError(errno.ENOSPC, "injected upload failure")

    def fail_write(*args, **kwargs):
        raise mutation_error

    def fail_unlink(*args, **kwargs):
        raise PermissionError(errno.EACCES, "injected staging unlink failure")

    try:
        with monkeypatch.context() as patch:
            patch.setattr(dummy_fs._client, "pwrite", fail_write)
            patch.setattr(dummy_fs, "rm", fail_unlink)
            with pytest.raises(OSError) as error:
                file.prepare()
            assert error.value is mutation_error
        assert dummy_fs.exists(file.temp_path)
        file.discard()
        assert not dummy_fs.exists(file.temp_path)
        assert not dummy_fs.exists("/destination")
        assert file._spool.closed
        assert not dummy_fs._client._fds
    finally:
        file.discard()


@pytest.mark.parametrize("failure_at", ["upload", "chmod"])
@pytest.mark.parametrize("transactional", [False, True])
@pytest.mark.parametrize(
    "cleanup_type", [PermissionError, KeyboardInterrupt, SystemExit, RuntimeError]
)
def test_prepare_preserves_primary_when_stage_cleanup_also_fails(
    dummy_fs, monkeypatch, failure_at, transactional, cleanup_type
):
    dummy_fs.pipe_file("/destination", b"original")
    primary = OSError(errno.ENOSPC, "preparation failed")
    cleanup = cleanup_type("stage cleanup failed")
    remove = dummy_fs.rm
    attempts = []
    file = None

    def fail_primary(*args, **kwargs):
        raise primary

    def fail_first_cleanup(path, *args, **kwargs):
        attempts.append(path)
        if len(attempts) == 1:
            raise cleanup
        return remove(path, *args, **kwargs)

    try:
        with monkeypatch.context() as patch:
            patch.setattr(
                dummy_fs._client,
                "pwrite" if failure_at == "upload" else "chmod",
                fail_primary,
            )
            patch.setattr(dummy_fs, "rm", fail_first_cleanup)
            with pytest.raises(BaseException) as caught:
                if transactional:
                    with dummy_fs.transaction:
                        with dummy_fs.open("/destination", "wb") as file:
                            file.write(b"replacement")
                else:
                    file = dummy_fs.open("/destination", "wb", autocommit=False)
                    file.write(b"replacement")
                    file.prepare()
            assert caught.value is primary
            if not issubclass(cleanup_type, OSError):
                assert caught.value.__cause__ is cleanup
        assert dummy_fs.cat_file("/destination") == b"original"
        assert file._stage_owned is (not transactional)
        file.discard()
        assert not dummy_fs.exists(file.temp_path)
        assert file._spool.closed
        assert not dummy_fs._client._fds
        assert attempts == [file.temp_path] * (2 if transactional else 1)
    finally:
        if file is not None:
            file.discard()
