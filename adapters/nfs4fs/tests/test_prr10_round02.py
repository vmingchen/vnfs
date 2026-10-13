"""PRR round 2: bulk cat retains range and expansion arguments."""

import fsspec
import pytest
from fsspec.callbacks import Callback


@pytest.mark.parametrize("selection", [["/a.txt", "/b.txt"], "/*.txt"])
@pytest.mark.parametrize("start,end", [(1, 3), (-3, None), (0, 0)])
def test_bulk_cat_applies_byte_ranges(dummy_fs, selection, start, end):
    payloads = {"/a.txt": b"abcdef", "/b.txt": b"ghijkl"}
    dummy_fs.pipe(payloads)
    assert dummy_fs.cat(selection, start=start, end=end) == {
        path: value[start:end] for path, value in payloads.items()
    }


@pytest.mark.parametrize("on_error", ["return", "omit", "raise"])
def test_bulk_cat_ranges_preserve_path_errors(dummy_fs, on_error):
    dummy_fs.pipe_file("/present", b"abcdef")
    if on_error == "raise":
        with pytest.raises(FileNotFoundError):
            dummy_fs.cat(["/present", "/missing"], start=1, end=3, on_error=on_error)
    else:
        result = dummy_fs.cat(
            ["/present", "/missing"], start=1, end=3, on_error=on_error
        )
        assert result["/present"] == b"bc"
        if on_error == "return":
            assert isinstance(result["/missing"], FileNotFoundError)
        else:
            assert "/missing" not in result


def test_bulk_cat_expands_list_globs_and_honors_recursive_depth(dummy_fs):
    dummy_fs.mkdir("/tree/deep", create_parents=True)
    dummy_fs.pipe({"/tree/direct.txt": b"direct", "/tree/deep/nested.txt": b"nested"})
    assert dummy_fs.cat(["/tree/*.txt"]) == {"/tree/direct.txt": b"direct"}
    assert dummy_fs.cat(["/tree"], recursive=True, maxdepth=1) == {
        "/tree/direct.txt": b"direct"
    }
    assert dummy_fs.cat(["/tree"], recursive=True, maxdepth=2) == {
        "/tree/direct.txt": b"direct",
        "/tree/deep/nested.txt": b"nested",
    }


@pytest.mark.parametrize("as_list", [False, True])
def test_bulk_cat_respects_explicit_literal_paths(dummy_fs, as_list):
    dummy_fs.pipe({"/literal[1].txt": b"literal", "/literal1.txt": b"expanded"})
    path = "/literal[1].txt"
    selection = [path] if as_list else path
    expected = {path: b"literal"} if as_list else b"literal"
    assert dummy_fs.cat(selection, assume_literal=True) == expected
    assert dummy_fs.expand_path(selection, assume_literal=True) == [path]


@pytest.mark.parametrize(
    "selection",
    [
        ["/none*", "/present"],
        ["/present", "/none*"],
        ["/p*", "/present"],
        ["/present", "/p*", "/present"],
    ],
    ids=["unmatched-first", "unmatched-last", "overlap", "repeated-overlap"],
)
@pytest.mark.parametrize("on_error", ["raise", "return", "omit"])
@pytest.mark.parametrize("ranged", [False, True], ids=["whole", "range"])
def test_bulk_cat_expands_selection_once(
    dummy_fs, tmp_path, monkeypatch, selection, on_error, ranged
):
    oracle_root = tmp_path / "oracle"
    oracle_root.mkdir()
    (oracle_root / "present").write_bytes(b"data")
    local = fsspec.filesystem("file")
    kwargs = {"start": 0, "end": 4} if ranged else {}
    expected = local.cat(
        [str(oracle_root / path.lstrip("/")) for path in selection],
        on_error=on_error,
        **kwargs,
    )
    expected = {
        "/" + path.removeprefix(str(oracle_root) + "/"): value
        for path, value in expected.items()
    }
    dummy_fs.pipe_file("/present", b"data")
    # Exactly enough for the unique selection, not duplicate selections.
    dummy_fs.read_all_max_total_bytes = 4
    method = "read_many" if ranged else "read_all_many"
    native_read = getattr(dummy_fs._client, method)
    calls = []

    def record(paths, *args, **kwargs):
        calls.append(list(paths))
        return native_read(paths, *args, **kwargs)

    monkeypatch.setattr(dummy_fs._client, method, record)
    callback = Callback()
    assert (
        dummy_fs.cat(selection, on_error=on_error, callback=callback, **kwargs)
        == expected
    )
    assert calls == [[dummy_fs._native_path("/present")]]
    assert (callback.size, callback.value) == (1, 1)


@pytest.mark.parametrize("recursive", [False, True])
def test_bulk_cat_deduplicates_without_scalar_reads_or_stats(
    dummy_fs, monkeypatch, recursive
):
    payloads = {"/tree/a": b"data", "/tree/b": b"more"}
    dummy_fs.mkdir("/tree")
    dummy_fs.pipe(payloads)
    dummy_fs.read_all_max_total_bytes = 8
    reads = []
    stats = []
    native_read = dummy_fs._client.read_all_many
    native_stat = dummy_fs._client.stat_many

    def record_read(paths, *args, **kwargs):
        reads.append(list(paths))
        return native_read(paths, *args, **kwargs)

    def record_stat(paths):
        stats.append(list(paths))
        return native_stat(paths)

    monkeypatch.setattr(dummy_fs._client, "read_all_many", record_read)
    monkeypatch.setattr(dummy_fs._client, "stat_many", record_stat)
    selection = (
        ["/tree", "/tree"]
        if recursive
        else ["/tree/b", "/tree/a", "nfs4:///tree/a", "//tree/a"]
    )
    assert dummy_fs.cat(selection, recursive=recursive) == payloads
    ordered_paths = list(payloads) if recursive else ["/tree/b", "/tree/a"]
    native_paths = [dummy_fs._native_path(path) for path in ordered_paths]
    assert reads == [native_paths]
    assert [batch for batch in stats if set(batch) & set(native_paths)] == [
        native_paths
    ]
