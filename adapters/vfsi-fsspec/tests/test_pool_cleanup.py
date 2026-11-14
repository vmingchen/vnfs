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


@pytest.mark.parametrize(
    "boundary", ["collection", "submission", "published_submission"]
)
@pytest.mark.parametrize("close_fails", [False, True])
def test_cancelled_independent_opens_close_or_defer_all_successes(
    monkeypatch, boundary, close_fails
):
    import vfsi_fsspec._fs as module

    class Native:
        def __init__(self):
            self.opens = []
            self.closes = []

        def open(self, path, mode):
            self.opens.append(path)
            return len(self.opens)

        def close_many(self, fds):
            self.closes.extend(fds)
            if close_fails:
                raise OSError("injected close failure")

        def shutdown(self):
            pass

    pool = _ClientPool(SimpleNamespace(NfsClient=Native), (), size=2)
    natives = [client._native for client in pool._clients]
    cancelled = KeyboardInterrupt("cancel parallel opens")

    def interrupted_collection(futures):
        for future in futures:
            future.result()
        raise cancelled
        yield  # Keep cancellation at iterator advancement, not construction.

    class InterruptedExecutor(ThreadPoolExecutor):
        def submit(self, fn, index):
            if boundary == "published_submission":
                super().submit(fn, index)
                raise cancelled
            if index == 1:
                raise cancelled
            return super().submit(fn, index)

    if boundary == "collection":
        monkeypatch.setattr(module, "as_completed", interrupted_collection)
    else:
        monkeypatch.setattr(module, "ThreadPoolExecutor", InterruptedExecutor)
    try:
        with pytest.raises(KeyboardInterrupt) as caught:
            pool.open_many_independent(["a", "b"], ["rb", "rb"])
        assert caught.value is cancelled
        count = 2 if boundary == "collection" else 1
        assert sum(len(native.opens) for native in natives) == count
        assert sum(len(native.closes) for native in natives) == count
        assert len(pool._fds) == (count if close_fails else 0)
        assert len(pool._deferred_close) == (count if close_fails else 0)
    finally:
        pool.shutdown()


@pytest.mark.parametrize("close_fails", [False, True])
def test_interrupted_executor_shutdown_defers_late_open_success(
    monkeypatch, close_fails
):
    import vfsi_fsspec._fs as module

    started = threading.Event()
    release = threading.Event()
    executors = []
    cancelled = KeyboardInterrupt("interrupted worker join")

    class Native:
        def __init__(self):
            self.opens = 0
            self.closes = []
            self.close_fails = close_fails

        def open(self, path, mode):
            self.opens += 1
            started.set()
            assert release.wait(5), "test did not release the pending open"
            return 11

        def close_many(self, fds):
            self.closes.append(list(fds))
            if self.close_fails:
                raise OSError("injected close failure")

        def shutdown(self):
            pass

    class InterruptedShutdown(ThreadPoolExecutor):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, **kwargs)
            self.interrupted = False
            executors.append(self)

        def submit(self, fn, index):
            if index == 1:
                raise RuntimeError("injected submission failure")
            return super().submit(fn, index)

        def shutdown(self, *args, **kwargs):
            if not self.interrupted:
                self.interrupted = True
                assert started.wait(5), "worker did not start"
                # Real ThreadPoolExecutor.join is interruptible by SIGINT.
                # Inject that boundary without signalling the pytest process.
                raise cancelled
            return super().shutdown(*args, **kwargs)

    pool = _ClientPool(SimpleNamespace(NfsClient=Native), (), size=1)
    native = pool._clients[0]._native
    monkeypatch.setattr(module, "ThreadPoolExecutor", InterruptedShutdown)
    try:
        with pytest.raises(KeyboardInterrupt) as caught:
            pool.open_many_independent(["first", "unsent"], ["rb", "rb"])
        assert caught.value is cancelled
        assert not pool._fds
        release.set()
        executors[0].shutdown(wait=True)
        assert native.opens == 1, "cancellation must not replay any open"
        assert not native.closes
        assert len(pool._fds) == len(pool._deferred_close) == 1
        pool._retry_deferred_closes()
        assert native.closes == [[11]]
        assert len(pool._fds) == len(pool._deferred_close) == int(close_fails)
        if close_fails:
            native.close_fails = False
            pool._retry_deferred_closes()
            assert native.closes == [[11], [11]]
            assert not pool._fds
            assert not pool._deferred_close
    finally:
        release.set()
        for executor in executors:
            executor.shutdown(wait=True)
        pool.shutdown()
