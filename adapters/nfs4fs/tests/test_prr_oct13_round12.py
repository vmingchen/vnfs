"""PRR round 12: same-client mutations invalidate cached inode aliases."""

import errno

import fsspec
import pytest
from fsspec.core import OpenFile
from vfsi_fsspec import _blockcache

ORIGINAL = b"original-long"
REPLACEMENT = b"changed"


def cache(fs, tmp_path):
    return fsspec.filesystem(
        "blockcache",
        fs=fs,
        cache_storage=str(tmp_path / "cache"),
        check_files=False,
        expiry_time=0,
        cache_check=0,
        skip_instance_cache=True,
    )


def warm(cached, path):
    with cached.open(path, "rb") as file:
        return file.read()


@pytest.mark.parametrize("alias", ["hardlink", "symlink"])
@pytest.mark.parametrize("operation", ["write", "truncate", "pipe", "group", "copy"])
def test_mutation_invalidates_content_and_listing_aliases(
    dummy_fs, tmp_path, alias, operation
):
    dummy_fs.makedirs("/left")
    dummy_fs.makedirs("/right")
    dummy_fs.pipe_file("/left/a", ORIGINAL)
    if alias == "hardlink":
        dummy_fs.hardlink("/left/a", "/right/b")
    else:
        dummy_fs.symlink("../left/a", "/right/b")
    cached = cache(dummy_fs, tmp_path)
    assert warm(cached, "/left/a") == ORIGINAL
    assert dummy_fs.ls("/left", detail=True)[0]["size"] == len(ORIGINAL)
    if operation == "write":
        with dummy_fs.open("/right/b", "rb+") as file:
            file.write(REPLACEMENT)
        expected = REPLACEMENT + ORIGINAL[len(REPLACEMENT) :]
    elif operation == "truncate":
        with dummy_fs.open("/right/b", "rb+") as file:
            file.truncate(3)
        expected = ORIGINAL[:3]
    elif operation == "pipe":
        dummy_fs.pipe_file("/right/b", REPLACEMENT)
        expected = REPLACEMENT
    elif operation == "group":
        dummy_fs.write_buffering = True
        files = dummy_fs.open_many([OpenFile(dummy_fs, "/right/b", "wb")])
        files[0].write(REPLACEMENT)
        dummy_fs.commit_many(files)
        expected = REPLACEMENT
    else:
        dummy_fs.pipe_file("/source", REPLACEMENT)
        dummy_fs.cp_file("/source", "/right/b")
        expected = REPLACEMENT
    assert dummy_fs.cat_file("/left/a") == expected
    assert warm(cached, "/left/a") == expected
    assert dummy_fs.ls("/left", detail=True)[0]["size"] == len(expected)
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("mode", ["rb+", "wb"])
@pytest.mark.parametrize("operation", ["write", "truncate"])
def test_descriptor_mutation_invalidates_original_inode_after_path_replacement(
    dummy_fs, tmp_path, mode, operation
):
    dummy_fs.pipe_file("/original", ORIGINAL)
    writer = dummy_fs.open("/original", mode)
    if mode == "wb":
        writer.write(ORIGINAL)
    dummy_fs.mv("/original", "/renamed")
    dummy_fs.pipe_file("/original", b"unrelated")
    cached = cache(dummy_fs, tmp_path)
    assert warm(cached, "/renamed") == ORIGINAL
    if operation == "write":
        writer.seek(0)
        writer.write(REPLACEMENT)
        expected = REPLACEMENT + ORIGINAL[len(REPLACEMENT) :]
    else:
        writer.truncate(3)
        expected = ORIGINAL[:3]
    writer.close()
    assert warm(cached, "/renamed") == expected
    assert dummy_fs.cat_file("/original") == b"unrelated"
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("rename", [False, True])
def test_group_flush_invalidates_cache_warmed_after_open(dummy_fs, tmp_path, rename):
    dummy_fs.pipe_file("/original", ORIGINAL)
    dummy_fs.write_buffering = True
    files = dummy_fs.open_many([OpenFile(dummy_fs, "/original", "ab")])
    destination = "/original"
    if rename:
        dummy_fs.mv("/original", "/renamed")
        dummy_fs.pipe_file("/original", b"unrelated")
        destination = "/renamed"
    cached = cache(dummy_fs, tmp_path)
    assert warm(cached, destination) == ORIGINAL
    files[0].write(REPLACEMENT)
    dummy_fs.commit_many(files)
    assert warm(cached, destination) == ORIGINAL + REPLACEMENT
    if rename:
        assert dummy_fs.cat_file("/original") == b"unrelated"


def test_uncached_mutations_keep_native_stat_free_path(dummy_fs, monkeypatch):
    calls = []
    stat = dummy_fs._client.stat_many

    def record(*args, **kwargs):
        calls.append(True)
        return stat(*args, **kwargs)

    monkeypatch.setattr(dummy_fs._client, "stat_many", record)
    dummy_fs.pipe({"/a": b"a", "/b": b"b"})
    assert not calls


def test_alias_invalidation_preserves_unrelated_cache_generation(dummy_fs, tmp_path):
    dummy_fs.pipe({"/a": ORIGINAL, "/unrelated": b"stable"})
    dummy_fs.hardlink("/a", "/alias")
    cached = cache(dummy_fs, tmp_path)
    assert warm(cached, "/a") == ORIGINAL
    assert warm(cached, "/unrelated") == b"stable"
    before = dict(cached._metadata.cached_files[-1]["/unrelated"])
    dummy_fs.pipe_file("/alias", REPLACEMENT)
    assert warm(cached, "/a") == REPLACEMENT
    assert cached._metadata.cached_files[-1]["/unrelated"] == before


@pytest.mark.parametrize("legacy", [False, True])
def test_alias_invalidation_falls_back_when_identity_is_unavailable(
    dummy_fs, tmp_path, monkeypatch, legacy
):
    dummy_fs.pipe_file("/a", ORIGINAL)
    dummy_fs.hardlink("/a", "/alias")
    cached = cache(dummy_fs, tmp_path)
    assert warm(cached, "/a") == ORIGINAL
    if legacy:
        cached._metadata.cached_files[-1]["/a"].pop("identity", None)
        _blockcache._save_writable_metadata(cached)

    def denied(*args):
        raise PermissionError(errno.EACCES, "metadata denied")

    with monkeypatch.context() as patch:
        if not legacy:
            patch.setattr(dummy_fs._client, "stat_many", denied)
        dummy_fs.pipe_file("/alias", REPLACEMENT)
    assert warm(cached, "/a") == REPLACEMENT


@pytest.mark.parametrize("alias", ["hardlink", "symlink"])
def test_alias_invalidation_preserves_already_open_populated_mapping(
    dummy_fs, tmp_path, alias
):
    dummy_fs.pipe_file("/a", ORIGINAL)
    if alias == "hardlink":
        dummy_fs.hardlink("/a", "/alias")
    else:
        dummy_fs.symlink("a", "/alias")
    cached = cache(dummy_fs, tmp_path)
    with cached.open("/a", "rb", block_size=2) as reader:
        assert reader.read(1) == ORIGINAL[:1]
    held = cached.open("/a", "rb", block_size=2)
    mapping = held.cache.cache
    assert held.read() == ORIGINAL
    held.seek(0)
    dummy_fs.pipe_file("/alias", REPLACEMENT)
    assert warm(cached, "/a") == REPLACEMENT
    assert not mapping.closed
    assert held.read() == ORIGINAL
    held.close()
    assert mapping.closed
    assert not dummy_fs._client._fds
