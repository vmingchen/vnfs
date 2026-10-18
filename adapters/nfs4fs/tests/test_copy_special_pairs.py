"""Every public copy route must classify special sources before reading."""

import subprocess
import sys
import textwrap

import pytest


@pytest.mark.parametrize("on_error", ["ignore", "raise"])
@pytest.mark.parametrize("symlinks", [False, True])
@pytest.mark.parametrize("indirect", [False, True])
@pytest.mark.parametrize("route", ["paired", "recursive"])
def test_special_copy_completes_without_a_fifo_peer(
    tmp_path, on_error, symlinks, indirect, route
):
    script = textwrap.dedent("""
        import os
        import sys
        from pathlib import Path

        import fsspec
        import nfs4fs
        from fsspec.callbacks import Callback

        remote = Path(sys.argv[1]) / "remote"
        source = remote / "source"
        source.mkdir(parents=True)
        (source / "alpha").write_bytes(b"payload")
        on_error, symlinks, indirect, route = sys.argv[2:]
        symlinks, indirect = symlinks == "True", indirect == "True"
        if indirect:
            os.mkfifo(remote / "fifo-target", 0o600)
            os.symlink("../fifo-target", source / "special")
        else:
            os.mkfifo(source / "special", 0o600)
        if route == "paired":
            (remote / "out").mkdir()
        with fsspec.filesystem(
            "nfs4", backend="dummy", dummy_root=str(remote),
            skip_instance_cache=True,
        ) as fs:
            copies = []
            original = fs._client.copy_many
            def record(pairs):
                copies.append(list(pairs))
                return original(pairs)
            fs._client.copy_many = record
            callback = Callback()
            def copy():
                if route == "paired":
                    fs.copy(
                        ["/source/special", "/source/alpha"],
                        ["/out/special", "/out/alpha"],
                        on_error=on_error, symlinks=symlinks, callback=callback,
                    )
                else:
                    fs.copy(
                        "/source", "/out", recursive=True,
                        on_error=on_error, symlinks=symlinks, callback=callback,
                    )
            rejected = not (indirect and symlinks)
            if rejected and on_error == "raise":
                try:
                    copy()
                except FileNotFoundError:
                    pass
                else:
                    raise AssertionError("special source was not rejected")
            else:
                copy()
            assert fs.cat_file("/out/alpha") == b"payload"
            if rejected:
                assert not (remote / "out/special").exists()
            else:
                assert (remote / "out/special").is_symlink()
                assert os.readlink(remote / "out/special") == "../fifo-target"
            assert copies == [[("/source/alpha", "/out/alpha")]], copies
            assert callback.size == callback.value == 2
            assert not fs._client._fds
        """)
    result = subprocess.run(
        [
            sys.executable,
            "-c",
            script,
            str(tmp_path),
            on_error,
            str(symlinks),
            str(indirect),
            route,
        ],
        capture_output=True,
        text=True,
        timeout=5,
    )
    assert result.returncode == 0, result.stdout + result.stderr


def test_regular_paired_copies_keep_one_metadata_and_copy_batch(
    dummy_fs, tmp_path, monkeypatch
):
    sources = [f"/source-{index}" for index in range(64)]
    destinations = [f"/copy-{index}" for index in range(64)]
    for index, source in enumerate(sources):
        (tmp_path / "root" / source[1:]).write_bytes(bytes([index]))
    attributes, copies = [], []
    stat_many = dummy_fs._client.stat_many
    copy_many = dummy_fs._client.copy_many

    def record_attributes(paths):
        attributes.append(list(paths))
        return stat_many(paths)

    def record_copies(pairs):
        copies.append(list(pairs))
        return copy_many(pairs)

    monkeypatch.setattr(dummy_fs._client, "stat_many", record_attributes)
    monkeypatch.setattr(dummy_fs._client, "copy_many", record_copies)
    dummy_fs.copy(sources, destinations)
    assert attributes == [sources]
    assert copies == [list(zip(sources, destinations))]
    for index, destination in enumerate(destinations):
        assert (tmp_path / "root" / destination[1:]).read_bytes() == bytes([index])


def test_regular_recursive_copies_reuse_walk_metadata(dummy_fs, tmp_path, monkeypatch):
    source = tmp_path / "root/source"
    source.mkdir()
    for index in range(64):
        (source / f"file-{index}").write_bytes(bytes([index]))
    attributes, copies = [], []
    stat_many = dummy_fs._client.stat_many
    copy_many = dummy_fs._client.copy_many

    def record_attributes(paths):
        attributes.append(list(paths))
        return stat_many(paths)

    def record_copies(pairs):
        copies.append(list(pairs))
        return copy_many(pairs)

    monkeypatch.setattr(dummy_fs._client, "stat_many", record_attributes)
    monkeypatch.setattr(dummy_fs._client, "copy_many", record_copies)
    dummy_fs.copy("/source", "/out", recursive=True)
    assert attributes == [["/source", "/out"]]
    assert len(copies) == 1 and len(copies[0]) == 64
    assert set(copies[0]) == {
        (f"/source/file-{index}", f"/out/file-{index}") for index in range(64)
    }
    for index in range(64):
        assert (tmp_path / "root/out" / f"file-{index}").read_bytes() == bytes([index])


def test_empty_paired_copy_does_not_probe_or_copy(dummy_fs, monkeypatch):
    def unexpected(*args):
        raise AssertionError("empty copy dispatched backend work")

    monkeypatch.setattr(dummy_fs._client, "stat_many", unexpected)
    monkeypatch.setattr(dummy_fs._client, "copy_many", unexpected)
    dummy_fs.copy([], [])


@pytest.mark.parametrize("target_kind", ["file", "directory"])
@pytest.mark.parametrize("on_error", ["raise", "ignore"])
def test_recursive_followed_links_keep_native_errors_and_copy_batch(
    dummy_fs, tmp_path, monkeypatch, target_kind, on_error
):
    remote = tmp_path / "root"
    (remote / "source").mkdir()
    (remote / "source/alpha").write_bytes(b"payload")
    if target_kind == "file":
        (remote / "target").write_bytes(b"target")
    else:
        (remote / "target").mkdir()
    (remote / "source/link").symlink_to("../target")
    attributes, copies = [], []
    stat_many = dummy_fs._client.stat_many
    copy_many = dummy_fs._client.copy_many

    def record_attributes(paths):
        attributes.append(list(paths))
        return stat_many(paths)

    def record_copies(pairs):
        copies.append(list(pairs))
        return copy_many(pairs)

    monkeypatch.setattr(dummy_fs._client, "stat_many", record_attributes)
    monkeypatch.setattr(dummy_fs._client, "copy_many", record_copies)

    def copy():
        dummy_fs.copy("/source", "/out", recursive=True, on_error=on_error)

    if target_kind == "directory" and on_error == "raise":
        with pytest.raises(IsADirectoryError):
            copy()
    else:
        copy()
    assert attributes == [["/source", "/out"], ["/source/link"]]
    assert len(copies) == 1
    assert set(copies[0]) == {
        ("/source/alpha", "/out/alpha"),
        ("/source/link", "/out/link"),
    }
    assert (remote / "out/alpha").read_bytes() == b"payload"
    if target_kind == "file":
        assert (remote / "out/link").read_bytes() == b"target"
    else:
        assert not (remote / "out/link").exists()


@pytest.mark.parametrize("directory_link", [False, True])
@pytest.mark.parametrize("on_error", ["raise", "ignore"])
def test_paired_special_filter_preserves_directory_errors_and_copy_cohort(
    dummy_fs, tmp_path, monkeypatch, directory_link, on_error
):
    import os

    from fsspec.callbacks import Callback

    remote = tmp_path / "root"
    (remote / "directory").mkdir()
    directory = "/directory"
    if directory_link:
        (remote / "directory-link").symlink_to("directory")
        directory = "/directory-link"
    os.mkfifo(remote / "fifo", 0o600)
    (remote / "alpha").write_bytes(b"payload")
    original = dummy_fs._client.copy_many
    copies = []

    def record(pairs):
        copies.append(list(pairs))
        assert all(source != "/fifo" for source, _ in pairs)
        return original(pairs)

    monkeypatch.setattr(dummy_fs._client, "copy_many", record)
    callback = Callback()

    def copy():
        dummy_fs.copy(
            [directory, "/fifo", "/alpha"],
            ["/out-directory", "/out-fifo", "/out-alpha"],
            on_error=on_error,
            callback=callback,
        )

    if on_error == "raise":
        with pytest.raises(IsADirectoryError):
            copy()
    else:
        copy()
    assert copies == [[(directory, "/out-directory"), ("/alpha", "/out-alpha")]]
    assert not (remote / "out-directory").exists()
    assert not (remote / "out-fifo").exists()
    assert (remote / "out-alpha").read_bytes() == b"payload"
    assert callback.size == callback.value == 3
    assert not dummy_fs._client._fds
