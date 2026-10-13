"""PRR round 5: persistent cache versions include filesystem identity."""

import fsspec
import pytest
from fsspec.core import OpenFile


@pytest.mark.parametrize("with_change", [False, True])
@pytest.mark.parametrize("grouped", [False, True])
def test_cache_rejects_same_fileid_from_another_filesystem(
    tmp_path, monkeypatch, with_change, grouped
):
    root = tmp_path / "remote"
    with fsspec.filesystem(
        "nfs4", backend="dummy", dummy_root=str(root), skip_instance_cache=True
    ) as fs:
        fs.pipe_file("/data", b"old data")
        stat_many = fs._client.stat_many
        filesystem_id = [0]

        def remote_metadata(paths):
            stats, errors = stat_many(paths)
            for attrs in stats:
                if attrs is None:
                    continue
                attrs.update(
                    fs_id=filesystem_id[0],
                    fileid=123,
                    modified_ns=456,
                    created_ns=789,
                    modified=0,
                    created=0,
                )
                if with_change:
                    attrs["change"] = 42
                else:
                    attrs.pop("change", None)
            return stats, errors

        monkeypatch.setattr(fs._client, "stat_many", remote_metadata)
        cached = fsspec.filesystem(
            "blockcache",
            fs=fs,
            cache_storage=str(tmp_path / "cache"),
            check_files=True,
            skip_instance_cache=True,
        )

        def read_cached():
            if grouped:
                entries = [OpenFile(cached, "/data", "rb")]
                files = cached.open_many(entries)
                try:
                    return files[0].read()
                finally:
                    # Finalize cache wrappers before their underlying group,
                    # as fsspec OpenFiles does when exiting a read context.
                    for entry in entries:
                        entry.close()
                    cached.commit_many(files)
            with cached.open("/data", "rb", block_size=4) as reader:
                return reader.read()

        assert read_cached() == b"old data"
        old_key = fs.ukey("/data")
        old_checksum = fs.checksum("/data")
        assert cached._metadata.cached_files[-1]["/data"]["uid"] == old_key
        # Simulate a server junction/remount with colliding object/version ids.
        (root / "data").write_bytes(b"new data")
        filesystem_id[0] = 1
        assert read_cached() == b"new data"
        assert fs.ukey("/data") != old_key
        assert fs.checksum("/data") != old_checksum
