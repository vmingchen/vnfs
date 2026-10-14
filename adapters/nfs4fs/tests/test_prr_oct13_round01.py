"""PRR round 1: reject malformed modes before namespace or data mutations."""

import pytest
from fsspec.core import OpenFile


@pytest.mark.parametrize("mode", ["wbb", "wbt", "wtt", "w++b", "xbb", "abtt"])
@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize("existing", [False, True])
def test_invalid_mode_preserves_files_and_missing_parents(
    dummy_fs, mode, grouped, existing
):
    dummy_fs.auto_mkdir = True
    dummy_fs.pipe_file("/sibling", b"sibling data")
    target = "/target" if existing else "/new/target"
    if existing:
        dummy_fs.pipe_file(target, b"target data")
    files = []
    try:
        with pytest.raises(ValueError):
            if grouped:
                files = dummy_fs.open_many(
                    [
                        OpenFile(dummy_fs, "/sibling", "wb"),
                        OpenFile(dummy_fs, target, mode),
                    ]
                )
            else:
                files = [dummy_fs.open(target, mode)]
    finally:
        if files:
            dummy_fs.commit_many(files)
    assert dummy_fs.cat_file("/sibling") == b"sibling data"
    if existing:
        assert dummy_fs.cat_file(target) == b"target data"
    else:
        assert not dummy_fs.exists("/new")
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("mode", ["wb", "wt", "w+b", "wb+"])
@pytest.mark.parametrize("grouped", [False, True])
def test_single_text_or_binary_mode_flag_remains_supported(dummy_fs, mode, grouped):
    files = (
        dummy_fs.open_many([OpenFile(dummy_fs, "/valid", mode)])
        if grouped
        else [dummy_fs.open("/valid", mode)]
    )
    try:
        assert files[0].write(b"value" if "b" in mode else "value") == 5
    finally:
        dummy_fs.commit_many(files)
    assert dummy_fs.cat_file("/valid") == b"value"
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("mode", ["wtt", "wbt", "wbb", "rtt", "rbt", "rbb"])
@pytest.mark.parametrize("grouped", [False, True])
def test_cache_wrapper_validates_original_mode_before_normalization(
    dummy_fs, tmp_path, mode, grouped
):
    import fsspec

    dummy_fs.pipe_file("/target", b"target data")
    cached = fsspec.filesystem(
        "blockcache",
        fs=dummy_fs,
        cache_storage=str(tmp_path / "cache"),
        skip_instance_cache=True,
    )
    with cached.open("/target", "rb", block_size=4) as file:
        assert file.read() == b"target data"
    assert cached._metadata.cached_files[-1]["/target"]["blocks"] is True
    files = []
    try:
        with pytest.raises(ValueError):
            files = (
                cached.open_many([OpenFile(cached, "/target", mode)])
                if grouped
                else [cached.open("/target", mode)]
            )
    finally:
        for file in files:
            file.close()
    assert dummy_fs.cat_file("/target") == b"target data"
    assert not dummy_fs._client._fds
