"""PRR round 2: bulk cat retains range and expansion arguments."""

import pytest


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
