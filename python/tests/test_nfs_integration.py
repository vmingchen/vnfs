"""Integration tests against the local NFSv4.1 server, including
round-trip (compound-count) assertions."""

import fsspec
import pytest

from .common import run_correctness_suite


def test_correctness_suite_on_nfs(nfs_fs):
    run_correctness_suite(nfs_fs)


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

    # pipe: write batch + truncate batch.
    _, pipe_count = _measured(
        fs, lambda: fs.pipe({p: f"data-{i}".encode() for i, p in enumerate(paths)})
    )
    assert pipe_count < 12, pipe_count

    # cat: one stat batch + one read batch.
    out, cat_count = _measured(fs, lambda: fs.cat(paths))
    assert len(out) == n
    assert cat_count < 12, cat_count

    # cat_ranges: one read batch (no stat).
    _, ranges_count = _measured(
        fs, lambda: fs.cat_ranges(paths, [0] * n, [6] * n)
    )
    assert ranges_count < 8, ranges_count

    # OpenFiles: one openv batch on enter, one closev batch on exit.
    open_files = fsspec.open_files(
        "nfs4:///rt/of_*.txt", mode="wb", num=n, host=fs.host, root=fs._root
    )
    fs._client.compound_stats()
    files = open_files.__enter__()
    open_count = fs._client.compound_stats()[0]
    assert open_count < 8, open_count
    for f in files:
        f.write(b"x" * 16)
    fs._client.compound_stats()
    open_files.__exit__(None, None, None)
    commit_count = fs._client.compound_stats()[0]
    assert commit_count < 3, commit_count

    # non-recursive rm: one removev batch.
    _, rm_count = _measured(fs, lambda: fs.rm(paths))
    assert rm_count < 6, rm_count

    # mv: one renamev batch.
    srcs = [_unique(nfs_fs, f"mv/{i}.txt") for i in range(n)]
    dsts = [_unique(nfs_fs, f"mv2/{i}.txt") for i in range(n)]
    fs.pipe({p: b"x" for p in srcs})
    _, mv_count = _measured(fs, lambda: fs.mv(srcs, dsts))
    assert mv_count < 8, mv_count

    # cp: size + read + write + truncate batches.
    _, cp_count = _measured(
        fs,
        lambda: fs.cp(
            dsts, [_unique(nfs_fs, f"cp/{i}.txt") for i in range(n)]
        ),
    )
    assert cp_count < 24, cp_count

    # walk on a 30-node tree is level-batched.
    for i in range(5):
        for j in range(5):
            fs.pipe_file(_unique(nfs_fs, f"tree/sub{i}/f{j}.txt"), b"x")
    _, walk_count = _measured(fs, lambda: list(fs.walk("nfs4:///tree")))
    assert walk_count < 12, walk_count


def test_round_trips_do_not_scale_with_file_count(nfs_fs):
    """Doubling the batch size must not double the compound count."""
    fs = nfs_fs
    fs.mkdir("nfs4:///scale", create_parents=True)

    def counts(n):
        paths = [_unique(nfs_fs, f"scale/{i}.txt") for i in range(n)]
        _, pipe_c = _measured(
            fs, lambda: fs.pipe({p: b"x" for p in paths})
        )
        _, cat_c = _measured(fs, lambda: fs.cat(paths))
        _, rm_c = _measured(fs, lambda: fs.rm(paths))
        return pipe_c, cat_c, rm_c

    small = counts(5)
    large = counts(20)
    for small_c, large_c in zip(small, large):
        assert large_c <= small_c + 4, (small, large)


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
