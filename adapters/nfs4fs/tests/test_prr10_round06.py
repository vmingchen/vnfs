"""PRR round 6: failed cached wrapper setup cleans every owned entry."""

import errno
import gzip

import fsspec
import pytest
from fsspec.core import OpenFile


@pytest.mark.parametrize("setup_error", ["missing", "encoding"])
@pytest.mark.parametrize("close_fails", [False, True])
def test_cached_open_many_setup_failure_cleans_partial_entry_and_siblings(
    dummy_fs, tmp_path, monkeypatch, setup_error, close_fails
):
    dummy_fs.pipe(
        {
            path: gzip.compress(data)
            for path, data in {
                "/first": b"first",
                "/second": b"second",
                "/third": b"third",
            }.items()
        }
    )
    cached = fsspec.filesystem(
        "blockcache",
        fs=dummy_fs,
        cache_storage=str(tmp_path / "cache"),
        skip_instance_cache=True,
    )
    entries = [
        OpenFile(cached, p, "rt", compression="gzip") for p in ("/first", "/second")
    ]
    entries.append(
        OpenFile(
            cached,
            "/missing" if setup_error == "missing" else "/third",
            "rt",
            compression="gzip",
            encoding="not-a-codec" if setup_error == "encoding" else "utf8",
        )
    )
    opened = []
    attempted = []
    native_open = dummy_fs._client.open
    native_close = dummy_fs._client.close
    cleanup_error = PermissionError(errno.EACCES, "injected CLOSE failure")

    def record_open(*args, **kwargs):
        fd = native_open(*args, **kwargs)
        opened.append(fd)
        return fd

    def record_close(fd):
        attempted.append(fd)
        if close_fails:
            raise cleanup_error
        return native_close(fd)

    try:
        with monkeypatch.context() as patch:
            patch.setattr(dummy_fs._client, "open", record_open)
            patch.setattr(dummy_fs._client, "close", record_close)
            expected = FileNotFoundError if setup_error == "missing" else LookupError
            with pytest.raises(expected) as error:
                cached.open_many(entries)
            assert not isinstance(error.value, PermissionError)
            assert set(attempted) == set(opened)
            assert set(dummy_fs._client._fds) <= dummy_fs._client._deferred_close
            if not close_fails:
                assert not dummy_fs._client._fds
        dummy_fs.info("/first")
        assert not dummy_fs._client._fds
    finally:
        for entry in entries:
            entry.close()
