"""A capability-aware correctness suite for every VFSI backend."""

import datetime
import os as _os

from vsmb import _native


def run_correctness_suite(fs):
    """Exercise the fsspec contract against `fs` (either backend)."""

    capabilities = fs._client.capabilities()
    posix_metadata = capabilities & _native.CAP_POSIX_METADATA != 0
    symlinks = capabilities & _native.CAP_SYMLINKS != 0
    hardlinks = capabilities & _native.CAP_HARDLINKS != 0
    non_utf8_paths = capabilities & _native.CAP_NON_UTF8_PATHS != 0

    # -- info / ls ---------------------------------------------------------
    info = fs.info("vsmbfs:///")
    expected = ["name", "type", "size", "mode", "modified", "islink"]
    if posix_metadata:
        expected.extend(["uid", "gid", "nlink", "fileid", "created", "checksum"])
    for key in expected:
        assert key in info, f"info missing {key}"
    assert info["type"] == "directory"
    assert info["name"].startswith("vsmbfs://")

    fs.mkdir("vsmbfs:///dir")
    fs.pipe({"vsmbfs:///dir/a.txt": b"alpha", "vsmbfs:///dir/b.txt": b"beta"})
    assert fs.exists("vsmbfs:///dir/a.txt")
    assert not fs.exists("vsmbfs:///dir/missing.txt")
    assert fs.isfile("vsmbfs:///dir/a.txt")
    assert fs.isdir("vsmbfs:///dir")
    assert not fs.isdir("vsmbfs:///dir/a.txt")

    # Arbitrary Unix filename bytes round-trip through Python's
    # surrogateescape representation.
    if non_utf8_paths:
        raw_name = b"n\xffb"
        name = _os.fsdecode(raw_name)
        fs.pipe_file(f"vsmbfs:///dir/{name}", b"bytes")
        assert fs.cat_file(f"vsmbfs:///dir/{name}") == b"bytes"
        assert (
            _os.fsencode(fs.ls("vsmbfs:///dir", detail=False)[-1].split("/")[-1])
            == raw_name
        )
        fs.rm(f"vsmbfs:///dir/{name}")

    plain = fs.ls("vsmbfs:///dir", detail=False)
    assert plain == ["vsmbfs:///dir/a.txt", "vsmbfs:///dir/b.txt"], plain
    detail = fs.ls("vsmbfs:///dir", detail=True)
    assert [d["name"] for d in detail] == plain
    a_info = [d for d in detail if d["name"].endswith("a.txt")][0]
    assert a_info["size"] == 5
    assert a_info["type"] == "file"

    # ls of a file returns [info(file)].
    file_ls = fs.ls("vsmbfs:///dir/a.txt", detail=True)
    assert len(file_ls) == 1 and file_ls[0]["name"].endswith("a.txt")

    # -- cat / cat_file / cat_ranges --------------------------------------
    assert fs.cat_file("vsmbfs:///dir/a.txt") == b"alpha"
    assert fs.cat_file("vsmbfs:///dir/a.txt", start=1, end=3) == b"lp"
    assert fs.cat_file("vsmbfs:///dir/a.txt", start=2) == b"pha"

    out = fs.cat(["vsmbfs:///dir/a.txt", "vsmbfs:///dir/b.txt"])
    assert set(out.values()) == {b"alpha", b"beta"}

    ranges = fs.cat_ranges(
        ["vsmbfs:///dir/a.txt", "vsmbfs:///dir/b.txt"], [0, 1], [3, 5]
    )
    assert ranges == [b"alp", b"eta"]

    # on_error semantics: missing paths yield exceptions per key.
    returned = fs.cat(
        ["vsmbfs:///dir/a.txt", "vsmbfs:///dir/missing.txt"], on_error="return"
    )
    assert returned["/dir/a.txt"] == b"alpha"
    assert isinstance(returned["/dir/missing.txt"], FileNotFoundError)
    omitted = fs.cat(
        ["vsmbfs:///dir/a.txt", "vsmbfs:///dir/missing.txt"], on_error="omit"
    )
    assert set(omitted) == {"/dir/a.txt"}
    try:
        fs.cat(["vsmbfs:///dir/a.txt", "vsmbfs:///dir/missing.txt"])
        raise AssertionError("cat with on_error=raise should raise")
    except FileNotFoundError:
        pass

    # -- pipe / pipe_file --------------------------------------------------
    fs.pipe({"vsmbfs:///p1.bin": b"1111", "vsmbfs:///p2.bin": b"22222"})
    assert fs.cat_file("vsmbfs:///p1.bin") == b"1111"
    # Overwriting with shorter data truncates (wb semantics).
    fs.pipe_file("vsmbfs:///p1.bin", b"x")
    assert fs.cat_file("vsmbfs:///p1.bin") == b"x"
    try:
        fs.pipe_file("vsmbfs:///p1.bin", b"y", mode="create")
        raise AssertionError("pipe_file mode=create on existing must raise")
    except FileExistsError:
        pass

    # -- open modes --------------------------------------------------------
    with fs.open("vsmbfs:///f.bin", "wb") as f:
        assert f.writable() and not f.readable()
        f.write(b"0123456789")
        assert f.tell() == 10
        f.seek(0)
        assert f.tell() == 0
    assert fs.size("vsmbfs:///f.bin") == 10

    with fs.open("vsmbfs:///f.bin", "rb") as f:
        assert f.readable() and not f.writable()
        assert f.seekable()
        assert f.read(4) == b"0123"
        assert f.tell() == 4
        assert f.seek(2, 1) == 6
        assert f.read(2) == b"67"
        assert f.seek(-3, 2) == 7
        assert f.read() == b"789"

    with fs.open("vsmbfs:///f.bin", "ab") as f:
        f.write(b"!")
    assert fs.cat_file("vsmbfs:///f.bin") == b"0123456789!"

    with fs.open("vsmbfs:///f.bin", "rb+") as f:
        f.seek(0)
        f.write(b"XX")
    assert fs.cat_file("vsmbfs:///f.bin") == b"XX23456789!"

    # Like LocalFileSystem, a direct open validates the path immediately.
    try:
        fs.open("vsmbfs:///dir", "rb")
        raise AssertionError("opening a directory for reading must raise")
    except IsADirectoryError:
        pass

    # close is idempotent.
    f = fs.open("vsmbfs:///dir/a.txt", "rb")
    assert f.read() == b"alpha"
    f.close()
    f.close()

    # -- mkdir / makedirs / rmdir / touch ----------------------------------
    fs.mkdir("vsmbfs:///a/b/c", create_parents=True)
    assert fs.isdir("vsmbfs:///a/b/c")
    fs.makedirs("vsmbfs:///a/b/c", exist_ok=True)
    try:
        fs.makedirs("vsmbfs:///a/b/c")
        raise AssertionError("makedirs without exist_ok on existing must raise")
    except FileExistsError:
        pass
    try:
        fs.mkdir("vsmbfs:///a/b/c", create_parents=False)
        raise AssertionError("mkdir of an existing dir must raise")
    except FileExistsError:
        pass
    try:
        fs.mkdir("vsmbfs:///no/such/parent/d", create_parents=False)
        raise AssertionError("mkdir without parents must raise")
    except FileNotFoundError:
        pass

    fs.touch("vsmbfs:///t.txt")
    assert fs.cat_file("vsmbfs:///t.txt") == b""
    fs.pipe_file("vsmbfs:///t.txt", b"longer content")
    fs.touch("vsmbfs:///t.txt", truncate=True)
    assert fs.size("vsmbfs:///t.txt") == 0

    fs.mkdir("vsmbfs:///empty-dir")
    fs.rmdir("vsmbfs:///empty-dir")
    try:
        fs.rmdir("vsmbfs:///dir")  # non-empty dir
        raise AssertionError("rmdir of a non-empty dir must raise")
    except OSError:
        pass

    # -- mv / cp -----------------------------------------------------------
    fs.pipe_file("vsmbfs:///mv-src.txt", b"mv-data")
    fs.mv("vsmbfs:///mv-src.txt", "vsmbfs:///mv-dst.txt")
    assert not fs.exists("vsmbfs:///mv-src.txt")
    assert fs.cat_file("vsmbfs:///mv-dst.txt") == b"mv-data"

    fs.mv("vsmbfs:///mv-dst.txt", "vsmbfs:///mv-dst2.txt")
    fs.pipe({"vsmbfs:///l1.txt": b"1", "vsmbfs:///l2.txt": b"2"})
    fs.mv(
        ["vsmbfs:///l1.txt", "vsmbfs:///l2.txt"],
        ["vsmbfs:///m1.txt", "vsmbfs:///m2.txt"],
    )
    assert fs.cat(["vsmbfs:///m1.txt", "vsmbfs:///m2.txt"]) == {
        "/m1.txt": b"1",
        "/m2.txt": b"2",
    }

    fs.cp("vsmbfs:///m1.txt", "vsmbfs:///c1.txt")
    assert fs.cat_file("vsmbfs:///c1.txt") == b"1"
    fs.cp(
        ["vsmbfs:///m1.txt", "vsmbfs:///m2.txt"],
        ["vsmbfs:///c2.txt", "vsmbfs:///c3.txt"],
    )
    assert fs.cat_file("vsmbfs:///c3.txt") == b"2"
    # Copying over a longer destination truncates the stale tail (O_TRUNC
    # semantics inside the write compound).
    fs.pipe_file("vsmbfs:///c4.txt", b"this-stale-tail-must-be-removed")
    fs.cp("vsmbfs:///m1.txt", "vsmbfs:///c4.txt")
    assert fs.cat_file("vsmbfs:///c4.txt") == b"1"

    # cp file onto an existing directory copies into it.
    fs.mkdir("vsmbfs:///cpdir")
    fs.cp("vsmbfs:///m1.txt", "vsmbfs:///cpdir")
    assert fs.cat_file("vsmbfs:///cpdir/m1.txt") == b"1"

    # -- recursive rm / cp -------------------------------------------------
    fs.mkdir("vsmbfs:///tree/inner", create_parents=True)
    fs.pipe_file("vsmbfs:///tree/inner/data.txt", b"xyz")
    if symlinks:
        fs.symlink("data.txt", "vsmbfs:///tree/inner/link")
    fs.cp("vsmbfs:///tree", "vsmbfs:///tree-copy", recursive=True)
    assert fs.cat_file("vsmbfs:///tree-copy/inner/data.txt") == b"xyz"
    # Default symlinks=False: links are copied through (regular files).
    if symlinks:
        assert fs.isfile("vsmbfs:///tree-copy/inner/link")
        assert fs.cat_file("vsmbfs:///tree-copy/inner/link") == b"xyz"

    try:
        fs.rm("vsmbfs:///tree", recursive=False)
        raise AssertionError("non-recursive rm of a dir must raise")
    except ValueError:
        pass
    fs.rm("vsmbfs:///tree", recursive=True)
    assert not fs.exists("vsmbfs:///tree")
    try:
        fs.rm("vsmbfs:///tree", recursive=True)
        raise AssertionError("rm of a missing path must raise")
    except FileNotFoundError:
        pass

    # -- links -------------------------------------------------------------
    fs.pipe_file("vsmbfs:///target.txt", b"linkdata")
    if symlinks:
        fs.symlink("target.txt", "vsmbfs:///rel-link")
        assert fs.readlink("vsmbfs:///rel-link") == "target.txt"
        rel_link = next(
            entry
            for entry in fs.ls("vsmbfs:///", detail=True)
            if entry["name"].endswith("/rel-link")
        )
        assert rel_link["islink"] is True
        # A dangling symlink still exists (lstat semantics).
        fs.symlink("no-such-target", "vsmbfs:///dangling")
        assert fs.exists("vsmbfs:///dangling")
    if hardlinks:
        fs.hardlink("vsmbfs:///target.txt", "vsmbfs:///hard.txt")
        assert (
            fs.info("vsmbfs:///target.txt")["fileid"]
            == fs.info("vsmbfs:///hard.txt")["fileid"]
        )

    # -- walk / find / glob / du ------------------------------------------
    fs.mkdir("vsmbfs:///wroot", create_parents=True)
    for sub in ("b", "a"):
        fs.mkdir(f"vsmbfs:///wroot/{sub}", create_parents=True)
        for fname in ("f1", "f2"):
            fs.pipe_file(f"vsmbfs:///wroot/{sub}/{fname}", b"x")
    fs.pipe_file("vsmbfs:///wroot/top.txt", b"y")

    walked = list(fs.walk("vsmbfs:///wroot"))
    by_dir = {d: (sorted(dirs), sorted(files)) for d, dirs, files in walked}
    assert set(by_dir) == {"/wroot", "/wroot/a", "/wroot/b"}
    assert by_dir["/wroot"] == (["a", "b"], ["top.txt"])
    assert by_dir["/wroot/a"] == ([], ["f1", "f2"])

    found = fs.find("vsmbfs:///wroot")
    assert len(found) == 5
    assert set(found) == {
        "/wroot/top.txt",
        "/wroot/a/f1",
        "/wroot/a/f2",
        "/wroot/b/f1",
        "/wroot/b/f2",
    }
    found_dirs = fs.find("vsmbfs:///wroot", withdirs=True)
    assert "/wroot/a" in found_dirs and "/wroot" in found_dirs
    assert len(fs.find("vsmbfs:///wroot", maxdepth=1)) == 1

    globbed = fs.glob("vsmbfs:///wroot/*/f1")
    assert sorted(globbed) == ["/wroot/a/f1", "/wroot/b/f1"]
    assert fs.glob("vsmbfs:///wroot/**/f2", maxdepth=3) == sorted(
        ["/wroot/a/f2", "/wroot/b/f2"]
    )

    assert fs.du("vsmbfs:///wroot") == 5  # five 1-byte files
    sizes = fs.du("vsmbfs:///wroot", total=False)
    assert sum(sizes.values()) == 5
    assert len(fs.du("vsmbfs:///wroot", total=False, withdirs=True)) == 8

    # -- metadata helpers --------------------------------------------------
    assert fs.size("vsmbfs:///target.txt") == 8
    if posix_metadata:
        assert isinstance(fs.created("vsmbfs:///target.txt"), datetime.datetime)
    assert isinstance(fs.modified("vsmbfs:///target.txt"), datetime.datetime)
    assert isinstance(fs.checksum("vsmbfs:///target.txt"), int)
    assert isinstance(fs.ukey("vsmbfs:///target.txt"), str)
    assert fs.ukey("vsmbfs:///target.txt") == fs.ukey("vsmbfs:///target.txt")

    # -- OpenFiles ---------------------------------------------------------
    from fsspec.core import OpenFile, OpenFiles

    def make_open_files(mode):
        return OpenFiles(
            [OpenFile(fs, f"vsmbfs:///of_{i}.bin", mode=mode) for i in range(3)],
            mode=mode,
            fs=fs,
        )

    opened = []
    original_open_many = fs.open_many
    original_commit_many = fs.commit_many

    def counting_open_many(of):
        opened.append(("open", len(of)))
        return original_open_many(of)

    def counting_commit_many(files):
        opened.append(("commit", len(files)))
        return original_commit_many(files)

    fs.open_many = counting_open_many
    fs.commit_many = counting_commit_many
    try:
        with make_open_files("wb") as files:
            assert isinstance(files, list)
            for i, f in enumerate(files):
                f.write(f"of-{i}".encode())
        assert ("open", 3) in opened and ("commit", 3) in opened
        opened.clear()
        with make_open_files("rb") as files:
            contents = [f.read() for f in files]
            for f in files:
                f.close()
        assert ("open", 3) in opened
        assert contents == [b"of-0", b"of-1", b"of-2"]
    finally:
        fs.open_many = original_open_many
        fs.commit_many = original_commit_many

    # -- get / put ---------------------------------------------------------
    import os
    import tempfile

    with tempfile.TemporaryDirectory() as td:
        local_file = os.path.join(td, "down.bin")
        fs.get("vsmbfs:///target.txt", local_file)
        with open(local_file, "rb") as fh:
            assert fh.read() == b"linkdata"

        fs.put(local_file, "vsmbfs:///up.bin")
        assert fs.cat_file("vsmbfs:///up.bin") == b"linkdata"

    # -- protocol / root handling ------------------------------------------
    assert fs._strip_protocol("vsmbfs:///x/y") == "/x/y"
    assert fs._strip_protocol("x/y") == "/x/y"
    assert fs._strip_protocol(["vsmbfs:///x", "vsmbfs:///y"]) == ["/x", "/y"]
    native = fs._native_path("/x/y")
    assert native.startswith("/") and native.endswith("/x/y")
    assert fs._internalize(native) == "/x/y"
