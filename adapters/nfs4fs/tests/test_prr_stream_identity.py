"""Parallel readers must pin one object across independently opened handles."""

import errno

import fsspec
import pytest


@pytest.fixture
def fs(tmp_path):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "root"),
        connection_pool_size=2,
        skip_instance_cache=True,
    ) as filesystem:
        yield filesystem


@pytest.mark.parametrize("replace_inode", [False, True])
def test_pipelined_worker_opens_pin_the_same_object(fs, monkeypatch, replace_inode):
    original = b"AAAABBBB"
    replacement = b"CCCCDDDD"
    fs.pipe_file("/data", original)
    open_many = fs._client.open_many

    def mutate_between_opens(paths, modes):
        first = open_many(paths[:1], modes[:1])
        if replace_inode:
            fs.mv("/data", "/retired")
        fs.pipe_file("/data", replacement)
        return first + open_many(paths[1:], modes[1:])

    monkeypatch.setattr(fs._client, "open_many_independent", mutate_between_opens)
    chunks = []

    def stream():
        return fs.read_stream_pipelined(
            "/data",
            lambda offset, data: chunks.append((offset, data)),
            workers=2,
            chunk_size=4,
            max_in_flight=1,
        )

    if replace_inode:
        with pytest.raises(OSError) as error:
            stream()
        assert error.value.errno == errno.ESTALE
        assert chunks == [(0, b"AAAA")]
        assert fs.cat_file("/retired") == original
    else:
        # In-place changes still use one object; this is not a content snapshot.
        assert stream() == len(replacement)
        assert chunks == [(0, b"CCCC"), (4, b"DDDD")]
    assert fs.cat_file("/data") == replacement
    assert not fs._client._fds


@pytest.mark.parametrize("missing_key", ["fs_id", "fileid"])
def test_pipelined_multiple_workers_require_verifiable_identity(
    fs, monkeypatch, missing_key
):
    fs.pipe_file("/data", b"AAAABBBB")
    fstat = fs._client.fstat

    def missing_identity(fd):
        attrs = fstat(fd)
        attrs.pop(missing_key, None)
        return attrs

    monkeypatch.setattr(fs._client, "fstat", missing_identity)
    chunks = []
    with pytest.raises(OSError) as error:
        fs.read_stream_pipelined(
            "/data",
            lambda offset, data: chunks.append((offset, data)),
            workers=2,
            chunk_size=4,
        )
    assert error.value.errno == errno.ESTALE
    assert chunks == []
    assert not fs._client._fds
