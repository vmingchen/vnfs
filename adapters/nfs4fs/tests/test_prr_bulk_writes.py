"""Production-review regressions for destructive bulk write recovery."""

import pytest


@pytest.fixture(params=["dummy_fs", "nfs_fs"])
def fs(request):
    return request.getfixturevalue(request.param)


def write_pair(fs, tmp_path, method):
    paths = ["/first", "/parent/second"]
    values = [b"first payload", b"second payload"]
    if method == "pipe":
        fs.pipe(dict(zip(paths, values)))
    else:
        sources = [tmp_path / "first", tmp_path / "second"]
        for path, value in zip(sources, values):
            path.write_bytes(value)
        fs.put([str(path) for path in sources], paths)


@pytest.mark.parametrize("method", ["pipe", "put"])
def test_bulk_auto_mkdir_prepares_parents_before_single_write(
    fs, tmp_path, monkeypatch, method
):
    fs.auto_mkdir = True
    original = fs._client.write_many
    calls = []

    def record(paths, values, **kwargs):
        calls.append(list(paths))
        return original(paths, values, **kwargs)

    monkeypatch.setattr(fs._client, "write_many", record)
    write_pair(fs, tmp_path, method)
    assert fs.cat_file("/first") == b"first payload"
    assert fs.cat_file("/parent/second") == b"second payload"
    assert calls == [[fs._native_path("/first"), fs._native_path("/parent/second")]]


@pytest.mark.parametrize("method", ["pipe", "put"])
def test_bulk_write_failure_does_not_replay_completed_mutations(
    fs, tmp_path, monkeypatch, method
):
    fs.auto_mkdir = True
    fs.mkdir("/parent")
    original = fs._client.write_many
    calls = []
    errors = []

    def remove_parent_then_write(paths, values, **kwargs):
        calls.append(list(paths))
        if len(calls) == 1:
            # Reproduce a parent disappearing between preparation and WRITE.
            fs.rmdir("/parent")
        try:
            return original(paths, values, **kwargs)
        except FileNotFoundError as error:
            errors.append(error)
            assert fs.cat_file("/first") == b"first payload"
            # Another writer updates an already-completed operation.
            original([fs._native_path("/first")], [b"other writer"], truncate=True)
            raise

    monkeypatch.setattr(fs._client, "write_many", remove_parent_then_write)
    with pytest.raises(FileNotFoundError) as caught:
        write_pair(fs, tmp_path, method)
    assert caught.value is errors[0]
    assert len(calls) == 1
    assert fs.cat_file("/first") == b"other writer"
    assert not fs.exists("/parent")
    assert not fs._client._fds
