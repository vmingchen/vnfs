"""Persistent-cache lock cleanup under filesystem errors."""

import errno
import fcntl
import os
from types import SimpleNamespace

import pytest

from vfsi_fsspec import _blockcache


def test_cache_lock_closes_descriptor_when_unlock_fails(tmp_path, monkeypatch):
    real_flock = fcntl.flock
    locked = []

    def fail_unlock(fd, operation):
        if operation == fcntl.LOCK_UN:
            raise OSError(errno.EINTR, "injected unlock failure")
        locked.append(fd)
        return real_flock(fd, operation)

    monkeypatch.setattr(_blockcache.fcntl, "flock", fail_unlock)
    try:
        with pytest.raises(OSError, match="injected unlock failure"):
            with _blockcache._cache_lock(SimpleNamespace(storage=[str(tmp_path)])):
                pass
        assert len(locked) == 1
        fd = locked[0]
        assert fd not in _blockcache._ACTIVE_LOCK_FDS
        with pytest.raises(OSError) as error:
            os.fstat(fd)
        assert error.value.errno == errno.EBADF
        # Closing the descriptor must also release its kernel lock.
        with open(tmp_path / ".vfsi-cache.lock", "rb") as probe:
            real_flock(probe.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    finally:
        # Keep the deliberately failing pre-fix reproduction isolated.
        for fd in locked:
            if fd in _blockcache._ACTIVE_LOCK_FDS:
                _blockcache._ACTIVE_LOCK_FDS.discard(fd)
                os.close(fd)
