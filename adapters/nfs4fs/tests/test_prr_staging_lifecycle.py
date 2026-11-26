"""Production-review regressions for staging ownership and file lifecycles."""

from contextlib import ExitStack

import pytest


@pytest.fixture(params=["dummy_fs", "nfs_fs"])
def fs(request):
    return request.getfixturevalue(request.param)


@pytest.mark.parametrize("mode", ["wb", "xb"])
@pytest.mark.parametrize("kind", ["file", "symlink"])
@pytest.mark.parametrize("failure", ["collision", "disconnect"])
def test_failed_stage_open_never_removes_unowned_path(
    fs, monkeypatch, mode, kind, failure
):
    file = fs.open("/destination", mode, autocommit=False)
    file.write(b"transaction payload")
    fs.pipe_file("/competitor", b"keep competitor")
    if kind == "file":
        fs.pipe_file(file.temp_path, b"keep staging name")
    else:
        fs.symlink("competitor", file.temp_path)
    native_open = fs._client.open

    def disconnect(path, mode, **kwargs):
        if path == fs._native_path(file.temp_path):
            raise ConnectionError("unacknowledged stage open")
        return native_open(path, mode, **kwargs)

    if failure == "disconnect":
        monkeypatch.setattr(fs._client, "open", disconnect)
    try:
        with pytest.raises(
            ConnectionError if failure == "disconnect" else FileExistsError
        ):
            file.commit()
        file.discard()
        if kind == "file":
            assert fs.cat_file(file.temp_path) == b"keep staging name"
        else:
            assert fs.readlink(file.temp_path) == "competitor"
        assert fs.cat_file("/competitor") == b"keep competitor"
        assert not fs.exists("/destination")
        assert not fs._client._fds
    finally:
        file.discard()


@pytest.mark.parametrize("outer_start", ["context", "manual"])
@pytest.mark.parametrize("inner_start", ["context", "manual"])
def test_nested_start_preserves_outer_transaction(fs, outer_start, inner_start):
    with ExitStack() as stack:
        if outer_start == "context":
            outer = stack.enter_context(fs.transaction)
        else:
            outer = fs.start_transaction()
            stack.push(outer)
        fs.pipe_file("/first", b"staged before nested start")

        with pytest.raises(RuntimeError, match="transaction is already active"):
            if inner_start == "context":
                with fs.transaction:
                    pass
            else:
                fs.start_transaction()

        assert fs.transaction is outer
        assert not fs.exists("/first")
        fs.pipe_file("/second", b"staged after nested start")

    assert fs.cat_file("/first") == b"staged before nested start"
    assert fs.cat_file("/second") == b"staged after nested start"
    assert not fs._client._fds


def test_transaction_remains_active_during_publication(dummy_fs, monkeypatch):
    with dummy_fs.transaction as transaction:
        dummy_fs.pipe_file("/target", b"staged")
        file = transaction.files[0]
        prepare = file.prepare

        def prepare_with_nested_start():
            with pytest.raises(RuntimeError, match="transaction is already active"):
                dummy_fs.start_transaction()
            return prepare()

        monkeypatch.setattr(file, "prepare", prepare_with_nested_start)

    assert dummy_fs.cat_file("/target") == b"staged"

    next_transaction = dummy_fs.start_transaction()
    assert next_transaction is not transaction
    dummy_fs.pipe_file("/next", b"next")
    assert not dummy_fs.exists("/next")
    dummy_fs.end_transaction()
    assert dummy_fs.cat_file("/next") == b"next"
