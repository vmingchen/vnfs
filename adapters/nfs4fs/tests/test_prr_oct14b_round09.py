"""Closing an unused child copy must abandon parent-owned native sessions."""

from types import SimpleNamespace

import pytest
from vfsi_fsspec import _fs


def pool_with_recorders(size, fail_abandon=False):
    natives = []

    class Native:
        def __init__(self, *args):
            self.shutdown_calls = 0
            self.abandon_calls = 0
            natives.append(self)

        def shutdown(self):
            self.shutdown_calls += 1

        def _abandon_after_fork(self):
            self.abandon_calls += 1
            if fail_abandon:
                raise RuntimeError("abandon failed")

    pool = _fs._ClientPool(SimpleNamespace(NfsClient=Native), (), size=size)
    return pool, natives


@pytest.mark.parametrize("size", [1, 3])
@pytest.mark.parametrize("forked", [False, True])
def test_pool_shutdown_distinguishes_parent_and_child_sessions(
    monkeypatch, size, forked
):
    pool, natives = pool_with_recorders(size)
    pid = _fs.os.getpid()
    with monkeypatch.context() as patch:
        if forked:
            patch.setattr(_fs.os, "getpid", lambda: pid + 1)
        pool.shutdown()
        pool.shutdown()
    assert pool.closed
    assert len(natives) == size  # Closing must not establish new sessions.
    assert [n.abandon_calls for n in natives] == [int(forked)] * size
    assert [n.shutdown_calls for n in natives] == [int(not forked)] * size


def test_public_filesystem_close_abandons_inherited_connection(dummy_fs, monkeypatch):
    dummy_fs._client.shutdown()
    pool, natives = pool_with_recorders(2)
    dummy_fs._client = pool
    pid = _fs.os.getpid()
    with monkeypatch.context() as patch:
        patch.setattr(_fs.os, "getpid", lambda: pid + 1)
        dummy_fs.close()
        dummy_fs.close()
    assert dummy_fs.closed
    assert [n.shutdown_calls for n in natives] == [0, 0]
    assert [n.abandon_calls for n in natives] == [1, 1]
    assert len(natives) == 2


def test_failed_child_abandon_still_closes_pool_and_attempts_all_owners(monkeypatch):
    pool, natives = pool_with_recorders(2, fail_abandon=True)
    pid = _fs.os.getpid()
    with monkeypatch.context() as patch:
        patch.setattr(_fs.os, "getpid", lambda: pid + 1)
        with pytest.raises(RuntimeError, match="abandon failed"):
            pool.shutdown()
        pool.shutdown()
    assert pool.closed
    assert [n.abandon_calls for n in natives] == [1, 1]
    assert [n.shutdown_calls for n in natives] == [0, 0]


def test_partial_fork_recovery_retires_only_inherited_sessions(monkeypatch):
    natives = []
    attempts = 0

    class Native:
        def __init__(self, *args):
            nonlocal attempts
            attempts += 1
            if attempts == 4:
                raise ConnectionError("second child connection failed")
            self.shutdown_calls = 0
            self.abandon_calls = 0
            natives.append(self)

        def shutdown(self):
            self.shutdown_calls += 1

        def _abandon_after_fork(self):
            self.abandon_calls += 1

    pool = _fs._ClientPool(SimpleNamespace(NfsClient=Native), (), size=2)
    pid = _fs.os.getpid()
    with monkeypatch.context() as patch:
        patch.setattr(_fs.os, "getpid", lambda: pid + 1)
        try:
            with pytest.raises(ConnectionError, match="second child connection failed"):
                pool.ensure_ready()
            assert len(natives) == 3
            assert pool._pid == pid
            pool.ensure_ready()
            assert pool._pid == pid + 1
        finally:
            pool.shutdown()

    assert [n.abandon_calls for n in natives[:2]] == [1, 1]
    assert [n.shutdown_calls for n in natives[:2]] == [0, 0]
    assert all(n.abandon_calls == 0 and n.shutdown_calls == 1 for n in natives[2:])
