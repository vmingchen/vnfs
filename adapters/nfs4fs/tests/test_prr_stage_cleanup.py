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
