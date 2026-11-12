"""Deferred cleanup must revalidate ownership after acquiring a session lock."""

import threading
from concurrent.futures import ThreadPoolExecutor
from types import SimpleNamespace

import pytest

from vfsi_fsspec._fs import _ClientPool


@pytest.mark.parametrize("action", ["shutdown", "retire_all", "retire_first"])
def test_deferred_cleanup_revalidates_tokens_after_waiting_for_owner(action):
    class Native:
        def __init__(self):
            self.closes = []
            self.shutdowns = 0

        def close_many(self, fds, *, _deferred=False):
            self.closes.append(list(fds))

        def shutdown(self):
            self.shutdowns += 1

    pool = _ClientPool(SimpleNamespace(NfsClient=Native), (), size=1)
    native = pool._clients[0]._native
    tokens = [pool._register(0, fd) for fd in [11, 12]]
    pool.defer_close_many(tokens)
    lock = pool._client_locks[0]
    waiting = threading.Event()
    owner = threading.get_ident()

    class ObservedLock:
        def __enter__(self):
            if threading.get_ident() != owner:
                waiting.set()
            return lock.__enter__()

        def __exit__(self, *args):
            return lock.__exit__(*args)

    pool._client_locks[0] = ObservedLock()
    try:
        with ThreadPoolExecutor(max_workers=1) as executor:
            with lock:
                cleanup = executor.submit(pool._retry_deferred_closes)
                assert waiting.wait(5), "cleanup did not reach the owner lock"
                if action == "shutdown":
                    pool.shutdown()
                else:
                    pool._discard_tokens(
                        tokens if action == "retire_all" else tokens[:1]
                    )
            cleanup.result(timeout=5)
        assert native.closes == ([[12]] if action == "retire_first" else [])
        assert not pool._fds
        assert not pool._deferred_close
        if action == "shutdown":
            assert pool.closed
            assert native.shutdowns == 1
    finally:
        pool.shutdown()
