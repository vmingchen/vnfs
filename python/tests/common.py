"""A correctness suite run against both the dummy and NFS backends."""

import datetime
import posixpath


def run_correctness_suite(fs):
    """Exercise the fsspec contract against `fs` (either backend)."""

    # -- info / ls ---------------------------------------------------------
    info = fs.info("nfs4:///")
    for key in (
        "name",
        "type",
        "size",
        "mode",
        "uid",
        "gid",
        "nlink",
        "fileid",
        "created",
        "modified",
        "checksum",
        "islink",
    ):
        assert key in info, f"info missing {key}"
    assert info["type"] == "directory"
    assert info["name"].startswith("nfs4://")

    fs.mkdir("nfs4:///dir")
    fs.pipe({"nfs4:///dir/a.txt": b"alpha", "nfs4:///dir/b.txt": b"beta"})
    assert fs.exists("nfs4:///dir/a.txt")
    assert not fs.exists("nfs4:///dir/missing.txt")
    assert fs.isfile("nfs4:///dir/a.txt")
    assert fs.isdir("nfs4:///dir")
    assert not fs.isdir("nfs4:///dir/a.txt")

    plain = fs.ls("nfs4:///dir", detail=False)
    assert plain == ["nfs4:///dir/a.txt", "nfs4:///dir/b.txt"], plain
    detail = fs.ls("nfs4:///dir", detail=True)
    assert [d["name"] for d in detail] == plain
    a_info = [d for d in detail if d["name"].endswith("a.txt")][0]
    assert a_info["size"] == 5
    assert a_info["type"] == "file"

    # ls of a file returns [info(file)].
    file_ls = fs.ls("nfs4:///dir/a.txt", detail=True)
    assert len(file_ls) == 1 and file_ls[0]["name"].endswith("a.txt")

    # -- cat / cat_file / cat_ranges --------------------------------------
    assert fs.cat_file("nfs4:///dir/a.txt") == b"alpha"
    assert fs.cat_file("nfs4:///dir/a.txt", start=1, end=3) == b"lp"
    assert fs.cat_file("nfs4:///dir/a.txt", start=2) == b"pha"

    out = fs.cat(["nfs4:///dir/a.txt", "nfs4:///dir/b.txt"])
    assert set(out.values()) == {b"alpha", b"beta"}

    ranges = fs.cat_ranges(["nfs4:///dir/a.txt", "nfs4:///dir/b.txt"], [0, 1], [3, 5])
    assert ranges == [b"alp", b"eta"]

    # on_error semantics: missing paths yield exceptions per key.
    returned = fs.cat(
        ["nfs4:///dir/a.txt", "nfs4:///dir/missing.txt"], on_error="return"
    )
    assert returned["/dir/a.txt"] == b"alpha"
    assert isinstance(returned["/dir/missing.txt"], FileNotFoundError)
    omitted = fs.cat(["nfs4:///dir/a.txt", "nfs4:///dir/missing.txt"], on_error="omit")
    assert set(omitted) == {"/dir/a.txt"}
    try:
        fs.cat(["nfs4:///dir/a.txt", "nfs4:///dir/missing.txt"])
        raise AssertionError("cat with on_error=raise should raise")
    except FileNotFoundError:
        pass

    # -- pipe / pipe_file --------------------------------------------------
    fs.pipe({"nfs4:///p1.bin": b"1111", "nfs4:///p2.bin": b"22222"})
    assert fs.cat_file("nfs4:///p1.bin") == b"1111"
    # Overwriting with shorter data truncates (wb semantics).
    fs.pipe_file("nfs4:///p1.bin", b"x")
    assert fs.cat_file("nfs4:///p1.bin") == b"x"
    try:
        fs.pipe_file("nfs4:///p1.bin", b"y", mode="create")
        raise AssertionError("pipe_file mode=create on existing must raise")
    except FileExistsError:
        pass

    # -- open modes --------------------------------------------------------
    with fs.open("nfs4:///f.bin", "wb") as f:
        assert f.writable() and not f.readable()
        f.write(b"0123456789")
        assert f.tell() == 10
        f.seek(0)
        assert f.tell() == 0
    assert fs.size("nfs4:///f.bin") == 10

    with fs.open("nfs4:///f.bin", "rb") as f:
        assert f.readable() and not f.writable()
        assert f.seekable()
        assert f.read(4) == b"0123"
        assert f.tell() == 4
        assert f.seek(2, 1) == 6
        assert f.read(2) == b"67"
        assert f.seek(-3, 2) == 7
        assert f.read() == b"789"

    with fs.open("nfs4:///f.bin", "ab") as f:
        f.write(b"!")
    assert fs.cat_file("nfs4:///f.bin") == b"0123456789!"

    with fs.open("nfs4:///f.bin", "rb+") as f:
        f.seek(0)
        f.write(b"XX")
    assert fs.cat_file("nfs4:///f.bin") == b"XX23456789!"

    # Reading a directory raises IsADirectoryError (lazy open on first I/O).
    f = fs.open("nfs4:///dir", "rb")
    try:
        f.read()
        raise AssertionError("reading a directory must raise")
    except IsADirectoryError:
        pass
    finally:
        f.close()

    # close is idempotent.
    f = fs.open("nfs4:///dir/a.txt", "rb")
    assert f.read() == b"alpha"
    f.close()
    f.close()

    # -- mkdir / makedirs / rmdir / touch ----------------------------------
    fs.mkdir("nfs4:///a/b/c", create_parents=True)
    assert fs.isdir("nfs4:///a/b/c")
    fs.makedirs("nfs4:///a/b/c", exist_ok=True)
    try:
        fs.makedirs("nfs4:///a/b/c")
        raise AssertionError("makedirs without exist_ok on existing must raise")
    except FileExistsError:
        pass
    try:
        fs.mkdir("nfs4:///a/b/c", create_parents=False)
        raise AssertionError("mkdir of an existing dir must raise")
    except FileExistsError:
        pass
    try:
        fs.mkdir("nfs4:///no/such/parent/d", create_parents=False)
        raise AssertionError("mkdir without parents must raise")
    except FileNotFoundError:
        pass

    fs.touch("nfs4:///t.txt")
    assert fs.cat_file("nfs4:///t.txt") == b""
    fs.pipe_file("nfs4:///t.txt", b"longer content")
    fs.touch("nfs4:///t.txt", truncate=True)
    assert fs.size("nfs4:///t.txt") == 0

    fs.mkdir("nfs4:///empty-dir")
    fs.rmdir("nfs4:///empty-dir")
    try:
        fs.rmdir("nfs4:///dir")  # non-empty dir
        raise AssertionError("rmdir of a non-empty dir must raise")
    except OSError:
        pass

    # -- mv / cp -----------------------------------------------------------
    fs.pipe_file("nfs4:///mv-src.txt", b"mv-data")
    fs.mv("nfs4:///mv-src.txt", "nfs4:///mv-dst.txt")
    assert not fs.exists("nfs4:///mv-src.txt")
    assert fs.cat_file("nfs4:///mv-dst.txt") == b"mv-data"

    fs.mv("nfs4:///mv-dst.txt", "nfs4:///mv-dst2.txt")
    fs.pipe({"nfs4:///l1.txt": b"1", "nfs4:///l2.txt": b"2"})
    fs.mv(["nfs4:///l1.txt", "nfs4:///l2.txt"], ["nfs4:///m1.txt", "nfs4:///m2.txt"])
    assert fs.cat(["nfs4:///m1.txt", "nfs4:///m2.txt"]) == {
        "/m1.txt": b"1",
        "/m2.txt": b"2",
    }

    fs.cp("nfs4:///m1.txt", "nfs4:///c1.txt")
    assert fs.cat_file("nfs4:///c1.txt") == b"1"
    fs.cp(["nfs4:///m1.txt", "nfs4:///m2.txt"], ["nfs4:///c2.txt", "nfs4:///c3.txt"])
    assert fs.cat_file("nfs4:///c3.txt") == b"2"

    # cp file onto an existing directory copies into it.
    fs.mkdir("nfs4:///cpdir")
    fs.cp("nfs4:///m1.txt", "nfs4:///cpdir")
    assert fs.cat_file("nfs4:///cpdir/m1.txt") == b"1"

    # -- recursive rm / cp -------------------------------------------------
    fs.mkdir("nfs4:///tree/inner", create_parents=True)
    fs.pipe_file("nfs4:///tree/inner/data.txt", b"xyz")
    fs.symlink("data.txt", "nfs4:///tree/inner/link")
    fs.cp("nfs4:///tree", "nfs4:///tree-copy", recursive=True)
    assert fs.cat_file("nfs4:///tree-copy/inner/data.txt") == b"xyz"
    # Default symlinks=False: links are copied through (regular files).
    assert fs.isfile("nfs4:///tree-copy/inner/link")
    assert fs.cat_file("nfs4:///tree-copy/inner/link") == b"xyz"

    try:
        fs.rm("nfs4:///tree", recursive=False)
        raise AssertionError("non-recursive rm of a dir must raise")
    except OSError:
        pass
    fs.rm("nfs4:///tree", recursive=True)
    assert not fs.exists("nfs4:///tree")
    try:
        fs.rm("nfs4:///tree", recursive=True)
        raise AssertionError("rm of a missing path must raise")
    except FileNotFoundError:
        pass

    # -- links -------------------------------------------------------------
    fs.pipe_file("nfs4:///target.txt", b"linkdata")
    fs.symlink("target.txt", "nfs4:///rel-link")
    assert fs.readlink("nfs4:///rel-link") == "target.txt"
    assert fs.ls("nfs4:///", detail=True)[0]["islink"] in (True, False)
    fs.hardlink("nfs4:///target.txt", "nfs4:///hard.txt")
    assert (
        fs.info("nfs4:///target.txt")["fileid"] == fs.info("nfs4:///hard.txt")["fileid"]
    )
    # A dangling symlink still exists (lstat semantics).
    fs.symlink("no-such-target", "nfs4:///dangling")
    assert fs.exists("nfs4:///dangling")

    # -- walk / find / glob / du ------------------------------------------
    fs.mkdir("nfs4:///wroot", create_parents=True)
    for sub in ("b", "a"):
        fs.mkdir(f"nfs4:///wroot/{sub}", create_parents=True)
        for fname in ("f1", "f2"):
            fs.pipe_file(f"nfs4:///wroot/{sub}/{fname}", b"x")
    fs.pipe_file("nfs4:///wroot/top.txt", b"y")

    walked = list(fs.walk("nfs4:///wroot"))
    by_dir = {d: (sorted(dirs), sorted(files)) for d, dirs, files in walked}
    assert set(by_dir) == {"/wroot", "/wroot/a", "/wroot/b"}
    assert by_dir["/wroot"] == (["a", "b"], ["top.txt"])
    assert by_dir["/wroot/a"] == ([], ["f1", "f2"])

    found = fs.find("nfs4:///wroot")
    assert len(found) == 5
    assert set(found) == {
        "/wroot/top.txt",
        "/wroot/a/f1",
        "/wroot/a/f2",
        "/wroot/b/f1",
        "/wroot/b/f2",
    }
    found_dirs = fs.find("nfs4:///wroot", withdirs=True)
    assert "/wroot/a" in found_dirs and "/wroot" in found_dirs
    assert len(fs.find("nfs4:///wroot", maxdepth=1)) == 1

    globbed = fs.glob("nfs4:///wroot/*/f1")
    assert sorted(globbed) == ["/wroot/a/f1", "/wroot/b/f1"]
    assert fs.glob("nfs4:///wroot/**/f2", maxdepth=3) == sorted(
        ["/wroot/a/f2", "/wroot/b/f2"]
    )

    assert fs.du("nfs4:///wroot") == 5  # five 1-byte files
    sizes = fs.du("nfs4:///wroot", total=False)
    assert sum(sizes.values()) == 5
    assert len(fs.du("nfs4:///wroot", total=False, withdirs=True)) == 8

    # -- metadata helpers --------------------------------------------------
    assert fs.size("nfs4:///target.txt") == 8
    assert isinstance(fs.created("nfs4:///target.txt"), datetime.datetime)
    assert isinstance(fs.modified("nfs4:///target.txt"), datetime.datetime)
    assert isinstance(fs.checksum("nfs4:///target.txt"), int)
    assert isinstance(fs.ukey("nfs4:///target.txt"), str)
    assert fs.ukey("nfs4:///target.txt") == fs.ukey("nfs4:///target.txt")

    # -- OpenFiles ---------------------------------------------------------
    from fsspec.core import OpenFile, OpenFiles

    def make_open_files(mode):
        return OpenFiles(
            [OpenFile(fs, f"nfs4:///of_{i}.bin", mode=mode) for i in range(3)],
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
        fs.get("nfs4:///target.txt", local_file)
        with open(local_file, "rb") as fh:
            assert fh.read() == b"linkdata"

        fs.put(local_file, "nfs4:///up.bin")
        assert fs.cat_file("nfs4:///up.bin") == b"linkdata"

    # -- protocol / root handling ------------------------------------------
    assert fs._strip_protocol("nfs4:///x/y") == "/x/y"
    assert fs._strip_protocol("x/y") == "/x/y"
    assert fs._strip_protocol(["nfs4:///x", "nfs4:///y"]) == ["/x", "/y"]
    native = fs._native_path("/x/y")
    assert native.startswith("/") and native.endswith("/x/y")
    assert fs._internalize(native) == "/x/y"
