"""Copy policy applies to special sources as well as backend errors."""

import os

import pytest
from fsspec.callbacks import Callback


@pytest.mark.parametrize("on_error", ["raise", "ignore"])
@pytest.mark.parametrize("symlinks", [False, True])
def test_fifo_copy_policy_preserves_healthy_siblings(
    dummy_fs, tmp_path, monkeypatch, on_error, symlinks
):
    os.mkfifo(tmp_path / "root" / "fifo", 0o600)
    dummy_fs.pipe_file("/alpha", b"payload")
    dummy_fs.mkdir("/out")
    original = dummy_fs._client.copy_many
    calls = []

    def record(pairs):
        calls.append(list(pairs))
        return original(pairs)

    monkeypatch.setattr(dummy_fs._client, "copy_many", record)
    callback = Callback()

    def copy():
        dummy_fs.copy(
            ["/fifo", "/alpha"],
            "/out/",
            on_error=on_error,
            symlinks=symlinks,
            callback=callback,
        )

    if on_error == "raise":
        with pytest.raises(FileNotFoundError, match="/fifo"):
            copy()
    else:
        copy()
    assert calls == [[("/alpha", "/out/alpha")]]
    assert dummy_fs.cat_file("/out/alpha") == b"payload"
    assert not dummy_fs.exists("/out/fifo")
    assert callback.size == callback.value == 2
    assert not dummy_fs._client._fds
