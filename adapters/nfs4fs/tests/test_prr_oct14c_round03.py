"""PRR round 3: preserve namespace path types, copy pairing, and source links."""

from pathlib import PurePosixPath

import fsspec
import pytest
from fsspec.callbacks import Callback


class DeclaredPath:
    """Expose a filesystem path independently of the display string."""

    def __init__(self, path):
        self.path = path

    def __fspath__(self):
        return self.path

    def __str__(self):
        return "/wrong-display-path"


PATH_TYPES = [str, PurePosixPath, DeclaredPath]


@pytest.mark.parametrize("path_type", PATH_TYPES)
@pytest.mark.parametrize("method", ["rm", "rm_file"])
def test_namespace_remove_accepts_scalar_pathlike(dummy_fs, path_type, method):
    dummy_fs.pipe_file("/target", b"remove me")
    dummy_fs.pipe_file("/wrong-display-path", b"keep me")

    getattr(dummy_fs, method)(path_type("/target"))

    assert not dummy_fs.exists("/target")
    assert dummy_fs.cat_file("/wrong-display-path") == b"keep me"


@pytest.mark.parametrize("path_type", PATH_TYPES)
def test_recursive_remove_accepts_scalar_pathlike(dummy_fs, path_type):
    dummy_fs.makedirs("/tree/nested")
    dummy_fs.pipe_file("/tree/nested/file", b"remove me")
    dummy_fs.pipe_file("/keep", b"keep me")

    dummy_fs.rm(path_type("/tree"), recursive=True)

    assert not dummy_fs.exists("/tree")
    assert dummy_fs.cat_file("/keep") == b"keep me"


@pytest.mark.parametrize("source_type", PATH_TYPES)
@pytest.mark.parametrize("destination_type", PATH_TYPES)
@pytest.mark.parametrize("existing_directory", [False, True])
def test_copy_scalar_pathlike_preserves_file_destination(
    dummy_fs, source_type, destination_type, existing_directory
):
    dummy_fs.pipe_file("/source", b"payload")
    if existing_directory:
        dummy_fs.mkdir("/destination")
    callback = Callback()

    dummy_fs.copy(
        source_type("/source"),
        destination_type("/destination"),
        callback=callback,
    )

    expected = "/destination/source" if existing_directory else "/destination"
    assert dummy_fs.cat_file(expected) == b"payload"
    assert dummy_fs.cat_file("/source") == b"payload"
    assert callback.value == callback.size == 1


@pytest.mark.parametrize("source_type", PATH_TYPES)
@pytest.mark.parametrize("destination_type", PATH_TYPES)
def test_recursive_copy_pathlike_preserves_directory_shape(
    dummy_fs, source_type, destination_type
):
    dummy_fs.makedirs("/source/nested")
    dummy_fs.pipe_file("/source/nested/file", b"payload")

    dummy_fs.copy(
        source_type("/source"),
        destination_type("/destination"),
        recursive=True,
        on_error="raise",
    )

    assert dummy_fs.cat_file("/destination/nested/file") == b"payload"
    assert not dummy_fs.exists("/destination/file")


@pytest.mark.parametrize("source_container", [list, tuple])
@pytest.mark.parametrize("destination_container", [list, tuple])
@pytest.mark.parametrize("duplicate_source", [False, True])
def test_copy_paired_sequences_preserve_order_and_duplicates(
    dummy_fs, source_container, destination_container, duplicate_source
):
    dummy_fs.pipe({"/a": b"AAA", "/b": b"BBB"})
    sources = ["/b", "/b" if duplicate_source else "/a"]
    callback = Callback()

    dummy_fs.copy(
        source_container(sources),
        destination_container(["/x", "/y"]),
        callback=callback,
    )

    assert dummy_fs.cat_file("/x") == b"BBB"
    assert dummy_fs.cat_file("/y") == (b"BBB" if duplicate_source else b"AAA")
    assert callback.value == callback.size == 2


@pytest.mark.parametrize("source_container", [list, tuple])
@pytest.mark.parametrize("destination_container", [list, tuple])
def test_copy_paired_sequences_validate_lengths_before_mutation(
    dummy_fs, source_container, destination_container
):
    dummy_fs.pipe({"/a": b"AAA", "/x": b"original"})

    with pytest.raises(ValueError, match="equal length"):
        dummy_fs.copy(
            source_container(["/a"]),
            destination_container(["/x", "/y"]),
        )

    assert dummy_fs.cat_file("/x") == b"original"
    assert not dummy_fs.exists("/y")


@pytest.mark.parametrize("source_slash", [False, True])
@pytest.mark.parametrize("destination_slash", [False, True])
def test_copy_custom_pathlike_preserves_trailing_separator_mapping(
    dummy_fs, source_slash, destination_slash
):
    dummy_fs.makedirs("/source/nested")
    dummy_fs.pipe_file("/source/nested/file", b"payload")
    dummy_fs.mkdir("/destination")
    source = "/source" + ("/" if source_slash else "")
    destination = "/destination" + ("/" if destination_slash else "")

    dummy_fs.copy(
        DeclaredPath(source),
        DeclaredPath(destination),
        recursive=True,
        on_error="raise",
    )

    expected = "/destination" if source_slash else "/destination/source"
    assert dummy_fs.cat_file(expected + "/nested/file") == b"payload"


@pytest.mark.parametrize("prefix", ["", "namespace"])
@pytest.mark.parametrize(
    "target_type,target",
    [
        (path_type, target)
        for path_type in PATH_TYPES
        for target in ["target", "/target", "nfs4:///target"]
        if path_type is not PurePosixPath or not target.startswith("nfs4:")
    ],
)
def test_symlink_pathlike_targets_preserve_relative_and_rooted_meaning(
    tmp_path, prefix, target_type, target
):
    remote = tmp_path / "remote"
    (remote / prefix).mkdir(parents=True)
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(remote),
        root=prefix,
        skip_instance_cache=True,
    ) as fs:
        fs.pipe_file("/target", b"payload")

        fs.symlink(target_type(target), "/link")

        expected = "target" if target == "target" else fs._native_path("/target")
        assert fs.readlink("/link") == expected
        assert fs.cat_file("/link") == b"payload"
        assert not fs._client._fds


@pytest.mark.parametrize("prefix", ["", "namespace"])
@pytest.mark.parametrize("target", ["real", "/real"])
@pytest.mark.parametrize("recursive", [False, True])
@pytest.mark.parametrize("source_type", [str, DeclaredPath])
def test_copy_preserves_scalar_directory_symlink(
    tmp_path, prefix, target, recursive, source_type
):
    remote = tmp_path / "remote"
    (remote / prefix).mkdir(parents=True)
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(remote),
        root=prefix,
        skip_instance_cache=True,
    ) as fs:
        fs.mkdir("/real")
        fs.pipe_file("/real/child", b"payload")
        fs.symlink(target, "/link")
        stored = fs.readlink("/link")
        callback = Callback()

        fs.copy(
            source_type("/link"),
            "/destination",
            recursive=recursive,
            symlinks=True,
            on_error="raise",
            callback=callback,
        )

        assert fs.readlink("/destination") == stored
        assert fs.cat_file("/destination/child") == b"payload"
        assert callback.value == callback.size == 1
        assert not fs._client._fds


@pytest.mark.parametrize("container", [list, tuple])
def test_copy_paired_pathlike_sequences_keep_one_native_batch(
    dummy_fs, monkeypatch, container
):
    dummy_fs.pipe({"/a": b"AAA", "/b": b"BBB"})
    copied = []
    original = dummy_fs._client.copy_many

    def record(pairs):
        copied.append(list(pairs))
        return original(pairs)

    monkeypatch.setattr(dummy_fs._client, "copy_many", record)

    dummy_fs.copy(
        container([DeclaredPath("/b"), PurePosixPath("/a")]),
        container([DeclaredPath("/x"), PurePosixPath("/y")]),
    )

    assert dummy_fs.cat_file("/x") == b"BBB"
    assert dummy_fs.cat_file("/y") == b"AAA"
    assert copied == [
        [
            (dummy_fs._native_path("/b"), dummy_fs._native_path("/x")),
            (dummy_fs._native_path("/a"), dummy_fs._native_path("/y")),
        ]
    ]


@pytest.mark.parametrize("source_link", [False, True])
def test_preserved_link_copy_follows_existing_destination_directory_link(
    dummy_fs, source_link
):
    dummy_fs.mkdir("/source")
    dummy_fs.pipe_file("/source/file", b"payload")
    dummy_fs.mkdir("/destination")
    dummy_fs.symlink("destination", "/alias")
    source = "/source"
    if source_link:
        dummy_fs.symlink("source", "/source-link")
        source = "/source-link"

    dummy_fs.copy(
        source,
        "/alias",
        recursive=True,
        symlinks=True,
        on_error="raise",
    )

    assert dummy_fs.readlink("/alias") == "destination"
    result = "/destination/" + source.rsplit("/", 1)[-1]
    if source_link:
        assert dummy_fs.readlink(result) == "source"
    else:
        assert dummy_fs.cat_file(result + "/file") == b"payload"
    assert not dummy_fs._client._fds


@pytest.mark.parametrize(
    "operation", ["rm", "copy-source", "copy-destination", "symlink"]
)
def test_namespace_pathlike_authority_is_checked_before_mutation(dummy_fs, operation):
    dummy_fs.pipe_file("/data", b"original")
    foreign = DeclaredPath("nfs4://other-server/data")

    with pytest.raises(ValueError, match="does not match"):
        if operation == "rm":
            dummy_fs.rm(foreign)
        elif operation == "copy-source":
            dummy_fs.copy(foreign, "/result")
        elif operation == "copy-destination":
            dummy_fs.copy("/data", foreign)
        else:
            dummy_fs.symlink(foreign, "/result")

    assert dummy_fs.cat_file("/data") == b"original"
    assert not dummy_fs.exists("/result")
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("source_type", PATH_TYPES)
@pytest.mark.parametrize("destination_container", [list, tuple])
@pytest.mark.parametrize("source_kind", ["file", "empty-directory", "directory"])
def test_copy_scalar_source_to_explicit_destination_sequence(
    dummy_fs, source_type, destination_container, source_kind
):
    destinations = ["/destination"]
    if source_kind == "file":
        dummy_fs.pipe_file("/source", b"payload")
    else:
        dummy_fs.mkdir("/source")
        if source_kind == "directory":
            dummy_fs.pipe_file("/source/file", b"payload")
            destinations.append("/destination/file")
    callback = Callback()

    dummy_fs.copy(
        source_type("/source"),
        destination_container(destinations),
        recursive=True,
        on_error="raise",
        callback=callback,
    )

    if source_kind == "file":
        assert dummy_fs.cat_file("/destination") == b"payload"
    else:
        assert dummy_fs.isdir("/destination")
        if source_kind == "directory":
            assert dummy_fs.cat_file("/destination/file") == b"payload"
    assert callback.value == callback.size == len(destinations)
    assert not dummy_fs._client._fds
