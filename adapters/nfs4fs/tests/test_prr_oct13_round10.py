"""PRR round 10: direct grouped codec handles own their binary cohort."""

import errno
import io

import pytest
from fsspec.core import OpenFile
from vfsi_fsspec import _fs

CODECS = [
    pytest.param(
        codec,
        marks=pytest.mark.skipif(codec not in _fs.compr, reason=f"{codec} unavailable"),
    )
    for codec in ["gzip", "bz2", "lzma", "xz", "zstd", "zip"]
]
PAYLOAD = b"group content\n"


def archive(codec):
    output = io.BytesIO()
    with _fs.compr[codec](output, mode="w") as file:
        file.write(PAYLOAD)
    return output.getvalue()


@pytest.mark.parametrize("codec", CODECS)
@pytest.mark.parametrize("text", [False, True])
@pytest.mark.parametrize("reverse", [False, True])
def test_direct_group_reader_close_releases_cohort(dummy_fs, codec, text, reverse):
    for path in ["/a", "/b"]:
        dummy_fs.pipe_file(path, archive(codec))
    mode = "rt" if text else "rb"
    entries = [
        OpenFile(dummy_fs, path, mode=mode, compression=codec) for path in ["/a", "/b"]
    ]
    files = dummy_fs.open_many(entries)
    raw = files.raw_files
    assert [file.read() for file in files] == (
        [PAYLOAD.decode()] * 2 if text else [PAYLOAD] * 2
    )
    order = list(reversed(files)) if reverse else files
    order[0].close()
    assert len(raw[0]._buffer_group._close_requested) == 1
    assert len(dummy_fs._client._fds) == 2
    order[1].close()
    assert all(file.closed for file in files)
    assert not dummy_fs._client._fds
    for file in files:
        file.close()
    dummy_fs.commit_many(files)
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("codec", CODECS)
@pytest.mark.parametrize("text", [False, True])
@pytest.mark.parametrize("buffered", [False, True])
def test_direct_group_writer_close_finishes_codecs(dummy_fs, codec, text, buffered):
    dummy_fs.write_buffering = buffered
    mode = "wt" if text else "wb"
    entries = [
        OpenFile(dummy_fs, path, mode=mode, compression=codec) for path in ["/a", "/b"]
    ]
    files = dummy_fs.open_many(entries)
    value = PAYLOAD.decode() if text else PAYLOAD
    for file in files:
        assert file.write(value) == len(value)
        file.close()
    assert not dummy_fs._client._fds
    for path in ["/a", "/b"]:
        with dummy_fs.open(path, "rb", compression=codec) as reader:
            assert reader.read() == PAYLOAD
    dummy_fs.commit_many(files)
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("completed", [0, 1])
@pytest.mark.parametrize("text", [False, True])
def test_group_codec_close_failure_defers_entire_remaining_cohort(
    dummy_fs, monkeypatch, completed, text
):
    for path in ["/a", "/b"]:
        dummy_fs.pipe_file(path, archive("gzip"))
    entries = [
        OpenFile(dummy_fs, path, mode="rt" if text else "rb", compression="gzip")
        for path in ["/a", "/b"]
    ]
    files = dummy_fs.open_many(entries)
    cleanup = OSError(errno.EIO, "group CLOSE failed")
    cleanup.index = completed
    close_many = dummy_fs._client.close_many
    calls = []

    def fail_close(tokens):
        calls.append(list(tokens))
        if completed:
            close_many(tokens[:completed])
        raise cleanup

    files[0].close()
    with monkeypatch.context() as patch:
        patch.setattr(dummy_fs._client, "close_many", fail_close)
        with pytest.raises(OSError) as error:
            files[1].close()
        assert error.value is cleanup
        assert len(calls) == 1
        assert len(dummy_fs._client._fds) == 2 - completed
        assert set(dummy_fs._client._fds) <= dummy_fs._client._deferred_close
        for file in files:
            file.close()
        assert len(calls) == 1
    dummy_fs.info("/a")
    assert not dummy_fs._client._fds
    dummy_fs.commit_many(files)
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("text", [False, True])
def test_group_cleanup_preserves_body_error(dummy_fs, monkeypatch, text):
    for path in ["/a", "/b"]:
        dummy_fs.pipe_file(path, archive("gzip"))
    entries = [
        OpenFile(dummy_fs, path, mode="rt" if text else "rb", compression="gzip")
        for path in ["/a", "/b"]
    ]
    files = dummy_fs.open_many(entries)
    files[0].close()
    primary = RuntimeError("application failed")
    cleanup = OSError(errno.EIO, "group CLOSE failed")

    def fail_close(*args):
        raise cleanup

    with monkeypatch.context() as patch:
        patch.setattr(dummy_fs._client, "close_many", fail_close)
        with pytest.raises(RuntimeError) as error:
            with files[1]:
                raise primary
        assert error.value is primary
        assert error.value.__cause__ is cleanup
        assert set(dummy_fs._client._fds) <= dummy_fs._client._deferred_close
    dummy_fs.info("/a")
    assert not dummy_fs._client._fds
    dummy_fs.commit_many(files)


@pytest.mark.parametrize("text", [False, True])
@pytest.mark.parametrize("failed_index", [0, 1])
def test_failed_member_flush_does_not_close_healthy_sibling(
    dummy_fs, monkeypatch, text, failed_index
):
    dummy_fs.write_buffering = True
    dummy_fs.block_size = 128
    entries = [
        OpenFile(dummy_fs, path, mode="wt" if text else "wb", compression="gzip")
        for path in ["/a", "/b"]
    ]
    files = dummy_fs.open_many(entries)
    value = PAYLOAD.decode() if text else PAYLOAD
    for file in files:
        file.write(value)
    primary = OSError(errno.EIO, "member WRITE failed")
    failed = files.raw_files[failed_index]
    healthy = files.raw_files[1 - failed_index]
    healthy_token = healthy._raw._fd

    def fail_write(*args):
        raise primary

    with monkeypatch.context() as patch:
        patch.setattr(dummy_fs._client, "pwrite_many", fail_write)
        with pytest.raises(OSError):
            files[failed_index].close()
        assert not files[1 - failed_index].closed
        assert healthy_token not in dummy_fs._client._deferred_close
        assert failed._raw._fd in dummy_fs._client._deferred_close
    dummy_fs.info("/a")
    assert dummy_fs._client.descriptor_valid(healthy_token)
    files[1 - failed_index].close()
    assert not dummy_fs._client._fds
    with dummy_fs.open(healthy.path, "rb", compression="gzip") as reader:
        assert reader.read() == PAYLOAD
    assert not dummy_fs._client._fds
    for file in files:
        file.close()
