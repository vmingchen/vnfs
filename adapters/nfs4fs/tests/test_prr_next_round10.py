"""PRR round 10: failed native shutdown still evicts closed filesystems."""

import fsspec
import pytest


def test_close_evicts_instance_cache_when_native_shutdown_raises(tmp_path, monkeypatch):
    options = {
        "backend": "dummy",
        "dummy_root": str(tmp_path / "cache"),
        "skip_instance_cache": False,
    }
    fs = fsspec.filesystem("nfs4", **options)
    original_shutdown = fs._client.shutdown

    def shutdown_then_fail():
        original_shutdown()
        raise OSError("injected native cleanup failure")

    monkeypatch.setattr(fs._client, "shutdown", shutdown_then_fail)
    with pytest.raises(OSError, match="injected native cleanup failure"):
        fs.close()

    assert fs.closed
    replacement = fsspec.filesystem("nfs4", **options)
    try:
        assert replacement is not fs
        assert not replacement.closed
    finally:
        replacement.close()
