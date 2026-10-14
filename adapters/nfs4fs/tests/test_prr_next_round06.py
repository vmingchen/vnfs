"""PRR round 6: aggregate sibling listings use the backend vector operation."""

import errno
import uuid

import pytest


def test_sibling_listing_batches_have_bounded_nfs_compound_count(nfs_fs):
    fs = nfs_fs
    root = f"/prr-vector-listdir-{uuid.uuid4().hex}"
    directories = [f"{root}/dir-{index}" for index in range(5)]
    fs.makedirs(root, exist_ok=True)
    for directory in directories:
        fs.makedirs(directory, exist_ok=True)
    fs.pipe(
        {
            f"{directory}/file-{index}": b"x"
            for directory in directories
            for index in range(5)
        }
    )

    fs._client.compound_stats()
    groups = fs._client.listdir_many(
        [fs._native_path(directory) for directory in directories],
        recursive=False,
        max_entries=25,
        max_path_bytes=4096,
    )
    compound_count = fs._client.compound_stats()[0]

    assert [len(group) for group in groups] == [5] * len(directories)
    assert compound_count <= 6, compound_count


def test_aggregate_vector_listing_preserves_duplicates_and_limits(dummy_fs):
    dummy_fs.makedirs("/directory", exist_ok=True)
    dummy_fs.pipe_file("/directory/file", b"x")

    groups = dummy_fs._client.listdir_many(
        ["/directory", "/directory"],
        recursive=False,
        max_entries=2,
        max_path_bytes=128,
    )
    assert [[entry["name"] for entry in group] for group in groups] == [
        ["/directory/file"],
        ["/directory/file"],
    ]

    with pytest.raises(OSError) as error:
        dummy_fs._client.listdir_many(
            ["/directory", "/directory"],
            recursive=False,
            max_entries=1,
            max_path_bytes=128,
        )
    assert error.value.errno == errno.EFBIG

    with pytest.raises(OSError) as error:
        dummy_fs._client.listdir_many(
            ["/directory"],
            recursive=False,
            max_entries=10,
            max_path_bytes=1,
        )
    assert error.value.errno == errno.EFBIG

    assert (
        dummy_fs._client.listdir_many(
            [], recursive=False, max_entries=0, max_path_bytes=0
        )
        == []
    )


def test_aggregate_vector_listing_remaps_error_to_original_path_index(dummy_fs):
    dummy_fs.makedirs("/present", exist_ok=True)
    dummy_fs.pipe_file("/present/file", b"x")

    with pytest.raises(FileNotFoundError) as error:
        dummy_fs._client.listdir_many(
            ["/present", "/present", "/missing"],
            recursive=False,
            max_entries=100,
            max_path_bytes=4096,
        )

    assert error.value.index == 2
    assert "/missing" in str(error.value)


def test_aggregate_vector_listing_budget_follows_original_path_order(dummy_fs):
    for directory in ("/a", "/b"):
        dummy_fs.makedirs(directory, exist_ok=True)
        dummy_fs.pipe_file(f"{directory}/file", b"x")

    with pytest.raises(OSError) as error:
        dummy_fs._client.listdir_many(
            ["/a", "/b", "/a"],
            recursive=False,
            max_entries=1,
            max_path_bytes=4096,
        )

    assert error.value.errno == errno.EFBIG
    assert error.value.index == 1
    assert "/b" in str(error.value)


def test_aggregate_budget_overflow_precedes_later_listing_error(dummy_fs):
    dummy_fs.makedirs("/a", exist_ok=True)
    dummy_fs.pipe_file("/a/first", b"x")
    dummy_fs.pipe_file("/a/second", b"x")

    with pytest.raises(OSError) as error:
        dummy_fs._client.listdir_many(
            ["/a", "/missing"],
            recursive=False,
            max_entries=1,
            max_path_bytes=4096,
        )

    assert error.value.errno == errno.EFBIG
    assert error.value.index == 0
    assert "/a" in str(error.value)


def test_nfs_aggregate_budget_overflow_precedes_later_resolution_error(nfs_fs):
    fs = nfs_fs
    root = f"/prr-budget-precedence-{uuid.uuid4().hex}"
    directory = f"{root}/a"
    fs.makedirs(directory, exist_ok=True)
    fs.pipe_file(f"{directory}/first", b"x")
    fs.pipe_file(f"{directory}/second", b"x")

    with pytest.raises(OSError) as error:
        fs._client.listdir_many(
            [fs._native_path(directory), fs._native_path(f"{root}/missing")],
            recursive=False,
            max_entries=1,
            max_path_bytes=4096,
        )

    assert error.value.errno == errno.EFBIG
    assert error.value.index == 0
    assert fs._native_path(directory) in str(error.value)


def test_aggregate_byte_overflow_stops_before_sibling_continuations(nfs_fs):
    fs = nfs_fs
    root = f"/prr-byte-stop-{uuid.uuid4().hex}"
    first, large = f"{root}/first", f"{root}/large"
    for directory in (first, large):
        fs.makedirs(directory, exist_ok=True)
    fs.pipe_file(f"{first}/entry", b"")
    fs.pipe({f"{large}/{index:04}-" + "x" * 220: b"" for index in range(1000)})

    fs._client.compound_stats()
    with pytest.raises(OSError) as scalar:
        fs._client.listdir(fs._native_path(first), max_entries=2000, max_path_bytes=1)
    scalar_compounds = fs._client.compound_stats()[0]
    assert scalar.value.errno == errno.EFBIG

    with pytest.raises(OSError) as aggregate:
        fs._client.listdir_many(
            [fs._native_path(first), fs._native_path(large)],
            recursive=False,
            max_entries=2000,
            max_path_bytes=1,
        )
    compounds = fs._client.compound_stats()[0]
    assert aggregate.value.errno == errno.EFBIG
    assert aggregate.value.index == 0
    # A bounded vector first wave and ordered overflow check are permitted,
    # but not fetching the large sibling's remaining READDIR pages.
    assert compounds <= scalar_compounds + 4, compounds


def test_paginated_aggregate_overflow_names_first_original_directory(nfs_fs):
    fs = nfs_fs
    root = f"/prr-page-order-{uuid.uuid4().hex}"
    directories = [f"{root}/d{index:02}" for index in range(15)]
    for directory in directories:
        fs.makedirs(directory, exist_ok=True)
    values = {f"{directories[0]}/{index:04}-" + "x" * 220: b"" for index in range(300)}
    values.update(
        {
            f"{directory}/{index:04}-" + "y" * 220: b""
            for directory in directories[1:]
            for index in range(12)
        }
    )
    fs.pipe(values)

    with pytest.raises(OSError) as error:
        fs._client.listdir_many(
            [fs._native_path(directory) for directory in directories],
            recursive=False,
            max_entries=250,
            max_path_bytes=16 * 1024 * 1024,
        )
    assert error.value.errno == errno.EFBIG
    assert error.value.index == 0
    assert fs._native_path(directories[0]) in str(error.value)
