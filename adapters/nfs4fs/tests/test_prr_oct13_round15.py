"""PRR round 15: namespace mutations invalidate directory alias caches."""

import errno

import fsspec
import pytest


@pytest.fixture(autouse=True)
def enable_listing_cache(dummy_fs):
    dummy_fs.dircache.use_listings_cache = True


def cached(fs, tmp_path):
    return fsspec.filesystem(
        "blockcache",
        fs=fs,
        cache_storage=str(tmp_path / "cache"),
        check_files=False,
        expiry_time=0,
        cache_check=0,
        skip_instance_cache=True,
    )


@pytest.mark.parametrize("existing", [False, True])
@pytest.mark.parametrize("operation", ["pipe", "mkdir", "open", "put", "copy", "touch"])
def test_new_child_through_directory_alias_refreshes_listing(
    dummy_fs, tmp_path, existing, operation
):
    dummy_fs.mkdir("/real")
    dummy_fs.symlink("real", "/alias")
    if existing:
        dummy_fs.pipe_file("/real/old", b"old")
    before = dummy_fs.ls("/real", detail=False)
    if operation == "pipe":
        dummy_fs.pipe_file("/alias/new", b"new")
    elif operation == "mkdir":
        dummy_fs.mkdir("/alias/new")
    elif operation == "open":
        with dummy_fs.open("/alias/new", "wb") as file:
            file.write(b"new")
    elif operation == "put":
        local = tmp_path / "upload"
        local.write_bytes(b"new")
        dummy_fs.put(str(local), "/alias/new")
    elif operation == "copy":
        dummy_fs.pipe_file("/source", b"new")
        dummy_fs.cp_file("/source", "/alias/new")
    else:
        dummy_fs.touch("/alias/new")
    assert dummy_fs.ls("/real", detail=False) == sorted(before + ["nfs4:///real/new"])


@pytest.mark.parametrize("operation", ["pipe", "mkdir"])
def test_real_path_mutation_refreshes_cached_alias_listing(dummy_fs, operation):
    dummy_fs.makedirs("/real/sub")
    dummy_fs.symlink("real", "/alias")
    assert dummy_fs.ls("/alias/sub", detail=False) == []
    if operation == "pipe":
        dummy_fs.pipe_file("/real/sub/new", b"new")
    else:
        dummy_fs.mkdir("/real/sub/new")
    assert dummy_fs.ls("/alias/sub", detail=False) == ["nfs4:///alias/sub/new"]


@pytest.mark.parametrize("cache_path", ["/real/sub/deep/file", "/alias/sub/deep/file"])
@pytest.mark.parametrize("source", ["/real/sub", "/alias/sub"])
def test_directory_rename_invalidates_persistent_descendant_aliases(
    dummy_fs, tmp_path, cache_path, source
):
    dummy_fs.makedirs("/real/sub/deep")
    dummy_fs.pipe_file("/real/sub/deep/file", b"original")
    dummy_fs.symlink("real", "/alias")
    cache = cached(dummy_fs, tmp_path)
    with cache.open(cache_path, "rb") as file:
        assert file.read() == b"original"
    assert dummy_fs.ls("/real/sub/deep", detail=False)
    dummy_fs.mv(source, "/alias/moved")
    assert not dummy_fs.exists(cache_path)
    with pytest.raises(FileNotFoundError):
        cache.open(cache_path, "rb")
    with pytest.raises(FileNotFoundError):
        dummy_fs.ls("/real/sub/deep")
    with cache.open("/real/moved/deep/file", "rb") as file:
        assert file.read() == b"original"


@pytest.mark.parametrize("target", ["real", "/real", "link"])
def test_directory_alias_resolution_keeps_unrelated_caches(dummy_fs, tmp_path, target):
    dummy_fs.makedirs("/real/sub")
    dummy_fs.makedirs("/other")
    dummy_fs.pipe_file("/other/stable", b"stable")
    dummy_fs.symlink("real", "/link")
    dummy_fs.symlink(target, "/alias")
    cache = cached(dummy_fs, tmp_path)
    with cache.open("/other/stable", "rb") as file:
        assert file.read() == b"stable"
    original = dict(cache._metadata.cached_files[-1]["/other/stable"])
    dummy_fs.ls("/other")
    dummy_fs.ls("/real/sub")
    dummy_fs.pipe_file("/alias/sub/new", b"new")
    assert "/other" in dummy_fs.dircache
    assert cache._metadata.cached_files[-1]["/other/stable"] == original
    assert dummy_fs.ls("/real/sub", detail=False) == ["nfs4:///real/sub/new"]


def test_namespace_alias_resolution_is_inactive_without_cached_state(
    dummy_fs, monkeypatch
):
    dummy_fs.auto_mkdir = True
    calls = []
    original = dummy_fs._client.lstat_many

    def record(*args):
        calls.append(True)
        return original(*args)

    monkeypatch.setattr(dummy_fs._client, "lstat_many", record)
    dummy_fs.pipe({"/a/file": b"a", "/b/file": b"b"})
    assert not calls


def test_namespace_auxiliary_resolution_failure_does_not_mask_mutation(
    dummy_fs, monkeypatch
):
    dummy_fs.makedirs("/real/sub")
    dummy_fs.symlink("real", "/alias")
    dummy_fs.ls("/real/sub")

    def denied(*args):
        raise PermissionError(errno.EACCES, "auxiliary metadata denied")

    with monkeypatch.context() as patch:
        patch.setattr(dummy_fs._client, "lstat_many", denied)
        dummy_fs.pipe_file("/alias/sub/new", b"new")
    assert dummy_fs.ls("/real/sub", detail=False) == ["nfs4:///real/sub/new"]


@pytest.mark.parametrize("outside_view", [False, True])
def test_namespace_aliases_respect_configured_prefix(tmp_path, outside_view):
    remote = tmp_path / "remote"
    (remote / "prefix").mkdir(parents=True)
    fs = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(remote),
        root="prefix",
        use_listings_cache=True,
        skip_instance_cache=True,
    )
    try:
        if outside_view:
            (remote / "outside" / "sub").mkdir(parents=True)
            fs.symlink("../outside", "/alias")
            listing = "/alias/sub"
        else:
            fs.makedirs("/real/sub")
            fs.symlink("real", "/alias")
            listing = "/real/sub"
        assert fs.ls(listing, detail=False) == []
        fs.pipe_file("/alias/sub/new", b"new")
        assert fs.ls(listing, detail=False) == [fs._fullpath(listing + "/new")]
    finally:
        fs.close()


@pytest.mark.parametrize("operation", ["remove", "rename"])
def test_namespace_resolution_preserves_final_symlink_semantics(
    dummy_fs, tmp_path, operation
):
    dummy_fs.makedirs("/real/sub")
    dummy_fs.pipe_file("/real/sub/file", b"original")
    dummy_fs.symlink("real", "/alias")
    cache = cached(dummy_fs, tmp_path)
    held = cache.open("/real/sub/file", "rb")
    assert held.read() == b"original"
    held.seek(0)
    dummy_fs.ls("/real/sub")
    if operation == "remove":
        dummy_fs.rm_file("/alias")
    else:
        dummy_fs.mv("/alias", "/renamed-link")
        assert dummy_fs.readlink("/renamed-link") == "real"
    assert not dummy_fs.exists("/alias")
    assert dummy_fs.cat_file("/real/sub/file") == b"original"
    assert held.read() == b"original"
    held.close()
    assert "/real/sub" in dummy_fs.dircache
    with cache.open("/real/sub/file", "rb") as file:
        assert file.read() == b"original"


@pytest.mark.parametrize("listing", ["/else/real/sub", "/alias/sub"])
def test_parent_component_after_intermediate_symlink_is_conservative(dummy_fs, listing):
    dummy_fs.makedirs("/else/sub")
    dummy_fs.makedirs("/else/real/sub")
    dummy_fs.mkdir("/real")
    dummy_fs.symlink("else/sub", "/jump")
    dummy_fs.symlink("jump/../real", "/alias")
    assert dummy_fs.ls(listing, detail=False) == []
    dummy_fs.pipe_file("/alias/sub/new", b"new")
    assert dummy_fs.cat_file("/else/real/sub/new") == b"new"
    assert dummy_fs.ls(listing, detail=False) == [dummy_fs._fullpath(listing + "/new")]


def test_parent_component_alias_rename_invalidates_persistent_descendants(
    dummy_fs, tmp_path
):
    dummy_fs.makedirs("/else/sub")
    dummy_fs.makedirs("/else/real/sub/deep")
    dummy_fs.mkdir("/real")
    dummy_fs.symlink("else/sub", "/jump")
    dummy_fs.symlink("jump/../real", "/alias")
    dummy_fs.pipe_file("/else/real/sub/deep/file", b"original")
    cache = cached(dummy_fs, tmp_path)
    with cache.open("/else/real/sub/deep/file", "rb") as file:
        assert file.read() == b"original"
    dummy_fs.mv("/alias/sub", "/alias/moved")
    assert not dummy_fs.exists("/else/real/sub/deep/file")
    with pytest.raises(FileNotFoundError):
        cache.open("/else/real/sub/deep/file", "rb")
