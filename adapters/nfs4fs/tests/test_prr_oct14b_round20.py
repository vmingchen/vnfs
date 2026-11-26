"""Single-file uploads share complete-write and exclusive-create guarantees."""

import errno

import fsspec
import pytest
from fsspec.callbacks import Callback


@pytest.mark.parametrize("accepted", [1, 2, 3])
def test_put_file_completes_short_writes(dummy_fs, tmp_path, monkeypatch, accepted):
    source = tmp_path / "source"
    source.write_bytes(b"abcdefgh")
    pwrite = dummy_fs._client.pwrite
    calls = []

    def short(fd, data, offset):
        calls.append(offset)
        return pwrite(fd, data[:accepted], offset)

    monkeypatch.setattr(dummy_fs._client, "pwrite", short)
    callback = Callback()
    dummy_fs.put_file(str(source), "/parent/target", callback=callback)
    assert dummy_fs.cat_file("/parent/target") == b"abcdefgh"
    assert callback.value == callback.size == 8
    assert len(calls) >= 3
    assert not dummy_fs._client._fds


def test_put_file_zero_progress_is_an_error(dummy_fs, tmp_path, monkeypatch):
    source = tmp_path / "source"
    source.write_bytes(b"abcdefgh")
    monkeypatch.setattr(dummy_fs._client, "pwrite", lambda *args: 0)
    with pytest.raises(OSError) as error:
        dummy_fs.put_file(str(source), "/target")
    assert error.value.errno == errno.EIO
    assert not dummy_fs._client._fds


def test_put_file_create_race_preserves_concurrent_winner(
    dummy_fs, tmp_path, monkeypatch
):
    source = tmp_path / "source"
    source.write_bytes(b"upload")
    original_open = dummy_fs.open
    raced = False

    def concurrent_create(path, mode="rb", **kwargs):
        nonlocal raced
        if path == "/target" and not raced:
            raced = True
            dummy_fs.pipe_file("/target", b"winner")
        return original_open(path, mode, **kwargs)

    monkeypatch.setattr(dummy_fs, "open", concurrent_create)
    with pytest.raises(FileExistsError):
        dummy_fs.put_file(str(source), "/target", mode="create")
    assert raced
    assert dummy_fs.cat_file("/target") == b"winner"
    assert not dummy_fs._client._fds


def test_put_file_invalid_mode_never_overwrites(dummy_fs, tmp_path):
    source = tmp_path / "source"
    source.write_bytes(b"upload")
    dummy_fs.pipe_file("/target", b"original")
    with pytest.raises(ValueError):
        dummy_fs.put_file(str(source), "/target", mode="invalid")
    assert dummy_fs.cat_file("/target") == b"original"


@pytest.mark.parametrize("mode", ["overwrite", "create"])
def test_put_file_success_and_empty_file_controls(dummy_fs, tmp_path, mode):
    source = tmp_path / "source"
    source.write_bytes(b"")
    callback = Callback()
    dummy_fs.put_file(str(source), "/parent/target", mode=mode, callback=callback)
    assert dummy_fs.cat_file("/parent/target") == b""
    assert callback.value == callback.size == 0
    assert not dummy_fs._client._fds


def test_put_file_missing_source_preserves_remote_destination(dummy_fs, tmp_path):
    dummy_fs.pipe_file("/target", b"original")
    with pytest.raises(FileNotFoundError):
        dummy_fs.put_file(str(tmp_path / "missing"), "/target")
    assert dummy_fs.cat_file("/target") == b"original"


def test_put_file_directory_remains_a_directory_operation(dummy_fs, tmp_path):
    source = tmp_path / "directory"
    source.mkdir()
    dummy_fs.put_file(str(source), "/parent/directory")
    assert dummy_fs.isdir("/parent/directory")
    with pytest.raises(FileExistsError):
        dummy_fs.put_file(str(source), "/parent/directory", mode="create")


@pytest.mark.parametrize("collision", [False, True])
def test_put_file_exclusively_creates_directory(
    dummy_fs, tmp_path, monkeypatch, collision
):
    source = tmp_path / "directory"
    source.mkdir()
    target = "/parent/target"
    ensure_dirs = dummy_fs._ensure_dirs

    def create_competing_directory(paths, **kwargs):
        if collision:
            dummy_fs._client.ensure_dir("/parent", 0o755)
            dummy_fs._client.mkdir(target, 0o700)
        return ensure_dirs(paths, **kwargs)

    monkeypatch.setattr(dummy_fs, "_ensure_dirs", create_competing_directory)
    if collision:
        with pytest.raises(FileExistsError):
            dummy_fs.put_file(source, target, mode="create")
        assert dummy_fs.info(target)["mode"] & 0o777 == 0o700
    else:
        dummy_fs.put_file(source, target, mode="create")
        assert dummy_fs.isdir(target)


@pytest.mark.parametrize("root", ["", "missing/root"])
def test_put_file_exclusive_directory_at_filesystem_root(tmp_path, root):
    source = tmp_path / "directory"
    source.mkdir()
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "backend"),
        root=root,
        skip_instance_cache=True,
    ) as fs:
        if root:
            fs.put_file(source, "/", mode="create")
            assert fs.isdir("/")
        with pytest.raises(FileExistsError):
            fs.put_file(source, "/", mode="create")


def test_put_file_exclusive_root_rejects_closed_filesystem(dummy_fs, tmp_path):
    dummy_fs.close()
    with pytest.raises(ValueError, match="filesystem is closed"):
        dummy_fs.put_file(tmp_path, "/", mode="create")


@pytest.mark.parametrize("commit", [False, True])
def test_put_file_respects_transaction_publication(dummy_fs, tmp_path, commit):
    source = tmp_path / "source"
    source.write_bytes(b"upload")
    dummy_fs.pipe_file("/target", b"original")
    transaction = dummy_fs.start_transaction()
    dummy_fs.put_file(str(source), "/target")
    assert dummy_fs.cat_file("/target") == b"original"
    transaction.complete(commit=commit)
    assert dummy_fs.cat_file("/target") == (b"upload" if commit else b"original")
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("commit", [False, True])
@pytest.mark.parametrize("compression", [None, "gzip"])
def test_transaction_put_file_defers_parent_creation(
    dummy_fs, tmp_path, commit, compression
):
    source = tmp_path / "source"
    source.write_bytes(b"upload")
    transaction = dummy_fs.start_transaction()
    dummy_fs.put_file(source, "/new-parent/target", compression=compression)
    assert not dummy_fs.exists("/new-parent")
    transaction.complete(commit=commit)
    if commit:
        with dummy_fs.open("/new-parent/target", "rb", compression=compression) as file:
            assert file.read() == b"upload"
    else:
        assert not dummy_fs.exists("/new-parent")
    assert not dummy_fs._client._fds


def test_put_file_creates_missing_configured_root(tmp_path):
    source = tmp_path / "source"
    source.write_bytes(b"upload")
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "backend"),
        root="new-root",
        skip_instance_cache=True,
    ) as fs:
        fs.put_file(source, "/target")
        assert fs.cat_file("/target") == b"upload"
