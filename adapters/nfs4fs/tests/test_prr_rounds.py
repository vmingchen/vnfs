"""Regressions from successive production readiness reviews."""

import pytest


@pytest.fixture(params=["dummy_fs", "nfs_fs"])
def fs(request):
    return request.getfixturevalue(request.param)


@pytest.mark.parametrize("name", ["a" * 255, "é" * 127], ids=["ascii", "utf8"])
@pytest.mark.parametrize("mode", ["wb", "ab", "xb"])
def test_transaction_accepts_long_destination_names(fs, name, mode):
    path = "/" + name
    # Establish that the backend accepts the destination name itself.
    fs.pipe_file(path, b"original")
    if mode == "xb":
        fs.rm_file(path)
    with fs.transaction:
        with fs.open(path, mode) as file:
            file.write(b"new")
    assert fs.cat_file(path) == (b"originalnew" if mode == "ab" else b"new")
    assert fs.ls("/", detail=False) == ["nfs4://" + path]
    assert not fs._client._fds


@pytest.mark.parametrize("mode", ["ab", "ab+"])
@pytest.mark.parametrize("buffered", [False, True])
@pytest.mark.parametrize("known_sizes", [False, True])
def test_grouped_append_starts_at_eof(fs, monkeypatch, mode, buffered, known_sizes):
    from fsspec.core import OpenFile

    paths = ["/append-a", "/append-b"]
    values = [b"original", b"longer-original"]
    fs.pipe(dict(zip(paths, values)))
    stat_many = fs._client.fstat_many
    calls = []

    def record(fds):
        calls.append(len(fds))
        return stat_many(fds)

    monkeypatch.setattr(fs._client, "fstat_many", record)
    files = fs.open_many(
        [OpenFile(fs, path, mode) for path in paths],
        write_buffering=buffered,
        sizes=[1, 1] if known_sizes else None,
    )
    try:
        # A caller-supplied read-size hint must not become an append position.
        assert [file.tell() for file in files] == list(map(len, values))
        for file in files:
            file.truncate()
            file.write(b"+")
    finally:
        fs.commit_many(files)
    assert [fs.cat_file(path) for path in paths] == [v + b"+" for v in values]
    assert calls == [2]
    assert not fs._client._fds


@pytest.mark.parametrize("cache_type", ["none", "readahead"])
def test_grouped_read_rejects_directory_and_closes_siblings(fs, cache_type):
    from fsspec.core import OpenFile

    fs.pipe_file("/file", b"original")
    fs.mkdir("/directory")
    opened = []
    try:
        with pytest.raises(IsADirectoryError):
            opened = fs.open_many(
                [OpenFile(fs, p, "rb") for p in ["/file", "/directory"]],
                cache_type=cache_type,
            )
    finally:
        fs.commit_many(opened)
    assert not fs._client._fds
    assert fs.cat_file("/file") == b"original"
