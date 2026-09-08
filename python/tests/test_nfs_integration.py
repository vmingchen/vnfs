"""Integration tests against the configured NFSv4.1/v4.2 server, including
round-trip (compound-count) assertions."""

import os

import fsspec
import pytest

from .common import run_correctness_suite


def test_correctness_suite_on_nfs(nfs_fs):
    run_correctness_suite(nfs_fs)


def test_nfs_identity_and_required_server_copy(nfs_fs):
    expected_minor = os.environ.get("VFSI_NFS_MINOR")
    if expected_minor:
        assert nfs_fs._client.minor_version() == int(expected_minor)

    if os.environ.get("VFSI_NFS_REQUIRE_SERVER_COPY") == "1":
        nfs_fs.pipe_file("nfs4:///server-copy-source", b"python-server-copy")
        nfs_fs.cp("nfs4:///server-copy-source", "nfs4:///server-copy-destination")
        assert (
            nfs_fs.cat_file("nfs4:///server-copy-destination") == b"python-server-copy"
        )
        assert nfs_fs._client.server_copy_enabled(), "Python copy used client fallback"


def _measured(fs, fn):
    """Run fn after resetting the compound counters; return (result, count)."""
    fs._client.compound_stats()
    result = fn()
    return result, fs._client.compound_stats()[0]


def _unique(nfs_fs, name):
    return f"nfs4:///{name}"


def test_round_trip_bounds(nfs_fs):
    """Bulk operations must use a bounded number of compounds regardless of
    how many files they touch (one-dir batches are constant)."""
    fs = nfs_fs
    n = 20
    for d in ("rt", "mv", "mv2", "cp", "tree"):
        fs.mkdir(f"nfs4:///{d}", create_parents=True)
    for i in range(5):
        fs.mkdir(f"nfs4:///tree/sub{i}", create_parents=True)
    paths = [_unique(nfs_fs, f"rt/{i}.txt") for i in range(n)]

    # pipe: one merged write compound with in-compound truncation.
    _, pipe_count = _measured(
        fs, lambda: fs.pipe({p: f"data-{i}".encode() for i, p in enumerate(paths)})
    )
    assert pipe_count == 1, pipe_count

    # cat: one merged no-stat read_allv compound.
    out, cat_count = _measured(fs, lambda: fs.cat(paths))
    assert len(out) == n
    assert cat_count == 1, cat_count

    # cat_ranges: one merged read compound (no stat).
    _, ranges_count = _measured(fs, lambda: fs.cat_ranges(paths, [0] * n, [6] * n))
    assert ranges_count == 1, ranges_count

    # OpenFiles: one merged openv compound on enter, one closev on exit.
    open_files = fsspec.open_files(
        "nfs4:///rt/of_*.txt", mode="wb", num=n, host=fs.host, root=fs._root
    )
    fs._client.compound_stats()
    files = open_files.__enter__()
    open_count = fs._client.compound_stats()[0]
    assert open_count == 1, open_count
    for f in files:
        f.write(b"x" * 16)
    fs._client.compound_stats()
    open_files.__exit__(None, None, None)
    commit_count = fs._client.compound_stats()[0]
    assert commit_count == 1, commit_count

    # non-recursive rm: one merged removev compound.
    _, rm_count = _measured(fs, lambda: fs.rm(paths))
    assert rm_count <= 2, rm_count

    # mv: one merged renamev compound.
    srcs = [_unique(nfs_fs, f"mv/{i}.txt") for i in range(n)]
    dsts = [_unique(nfs_fs, f"mv2/{i}.txt") for i in range(n)]
    fs.pipe({p: b"x" for p in srcs})
    _, mv_count = _measured(fs, lambda: fs.mv(srcs, dsts))
    assert mv_count == 1, mv_count

    # cp: no-stat read_allv + one truncating writev compound.
    _, cp_count = _measured(
        fs,
        lambda: fs.cp(dsts, [_unique(nfs_fs, f"cp/{i}.txt") for i in range(n)]),
    )
    assert cp_count == 2, cp_count

    # walk on a 30-node tree is level-batched.
    for i in range(5):
        for j in range(5):
            fs.pipe_file(_unique(nfs_fs, f"tree/sub{i}/f{j}.txt"), b"x")
    _, walk_count = _measured(fs, lambda: list(fs.walk("nfs4:///tree")))
    assert walk_count <= 6, walk_count


def test_recursive_tree_ops_are_batched(nfs_fs):
    """Recursive rm/copy must walk once and batch per level instead of paying
    per-file compounds."""
    fs = nfs_fs
    for i in range(4):
        fs.mkdir(f"nfs4:///rtree/d{i}", create_parents=True)
    fs.pipe({f"nfs4:///rtree/d{i}/f{j}.txt": b"x" for i in range(4) for j in range(4)})

    _, copy_count = _measured(
        fs, lambda: fs.copy("nfs4:///rtree", "nfs4:///rtree-copy", recursive=True)
    )
    assert copy_count < 60, copy_count
    assert len(fs.find("nfs4:///rtree-copy")) == 16

    _, rm_count = _measured(fs, lambda: fs.rm("nfs4:///rtree-copy", recursive=True))
    assert rm_count < 20, rm_count


def test_round_trips_do_not_scale_with_file_count(nfs_fs):
    """Doubling the batch size must not double the compound count."""
    fs = nfs_fs
    fs.mkdir("nfs4:///scale", create_parents=True)

    def counts(n):
        paths = [_unique(nfs_fs, f"scale/{i}.txt") for i in range(n)]
        _, pipe_c = _measured(fs, lambda: fs.pipe({p: b"x" for p in paths}))
        _, cat_c = _measured(fs, lambda: fs.cat(paths))
        _, rm_c = _measured(fs, lambda: fs.rm(paths))
        return pipe_c, cat_c, rm_c

    small = counts(5)
    large = counts(20)
    for small_c, large_c in zip(small, large):
        assert large_c <= small_c + 1, (small, large)


def test_open_files_reads_are_batched(nfs_fs):
    fs = nfs_fs
    fs.mkdir("nfs4:///ofr", create_parents=True)
    paths = [_unique(nfs_fs, f"ofr/{i}.txt") for i in range(10)]
    fs.pipe({p: b"hello" for p in paths})
    open_files = fsspec.open_files(
        "nfs4:///ofr/*.txt", mode="rb", host=fs.host, root=fs._root
    )
    fs._client.compound_stats()
    files = open_files.__enter__()
    enter_count = fs._client.compound_stats()[0]
    assert enter_count < 8, enter_count
    contents = [f.read() for f in files]
    for f in files:
        f.close()
    assert contents == [b"hello"] * 10


def test_compound_size_limit_is_configurable(nfs_fs):
    """fsspec.filesystem('nfs4', compound_size_limit=...) controls the
    per-compound payload cap: a small cap splits one pipe into several
    compounds, and the data still round-trips."""
    fs = fsspec.filesystem(
        "nfs4",
        host="127.0.0.1",
        root=nfs_fs._root + "_limit",
        compound_size_limit=64 * 1024,
    )
    fs.mkdir("nfs4:///", create_parents=True)
    try:
        paths = [f"nfs4:///f{i}.txt" for i in range(4)]
        data = b"x" * 64 * 1024
        fs._client.compound_stats()
        fs.pipe({p: data for p in paths})
        count = fs._client.compound_stats()[0]
        # 64 KiB writes under a 64 KiB cap: one write compound per file,
        # with the truncate fused into each write compound (no separate
        # truncate round trip).
        assert count >= 4 and count <= 6, count
        for p in paths:
            assert fs.cat_file(p) == data
    finally:
        fs.rm("nfs4:///", recursive=True)
