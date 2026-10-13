"""PRR round 3: reject unwritable buffers before cached read I/O."""

from array import array

import pytest
from fsspec.core import OpenFile


@pytest.mark.parametrize("cache_type", ["readahead", "bytes", "blockcache"])
@pytest.mark.parametrize("grouped", [False, True])
def test_cached_readinto_rejects_readonly_buffer_without_consuming(
    dummy_fs, monkeypatch, cache_type, grouped
):
    dummy_fs.pipe_file("/data", b"ABCDEFGH")
    files = (
        dummy_fs.open_many([OpenFile(dummy_fs, "/data", "rb")], cache_type=cache_type)
        if grouped
        else [dummy_fs.open("/data", "rb", cache_type=cache_type)]
    )
    reader = files[0]
    calls = []
    pread = dummy_fs._client.pread
    pread_many = dummy_fs._client.pread_many

    def record(*args):
        calls.append(args)
        return pread(*args)

    def record_many(*args):
        calls.append(args)
        return pread_many(*args)

    monkeypatch.setattr(dummy_fs._client, "pread", record)
    monkeypatch.setattr(dummy_fs._client, "pread_many", record_many)
    try:
        with pytest.raises(TypeError):
            reader.readinto(memoryview(b"read only"))
        assert reader.tell() == 0
        assert calls == []
        target = array("I", [0, 0])
        assert reader.readinto(target) == 8
        assert target.tobytes() == b"ABCDEFGH"
    finally:
        dummy_fs.commit_many(files)
