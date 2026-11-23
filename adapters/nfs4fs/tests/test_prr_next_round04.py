"""PRR round 4: concurrent parent creation should not break mkdir-p writes."""

import errno
import uuid

import pytest


@pytest.fixture(params=["dummy_fs", "nfs_fs"])
def fs(request):
    return request.getfixturevalue(request.param)


def test_auto_mkdir_accepts_parent_created_after_existence_probe(fs, monkeypatch):
    fs.auto_mkdir = True
    parent = f"/prr-mkdir-race-{uuid.uuid4().hex}"
    target = f"{parent}/file"
    native_parent = fs._native_path(parent)
    original_exists_many = fs._client.exists_many
    raced = []

    def create_parent_after_probe(paths):
        results = original_exists_many(paths)
        if (
            native_parent in paths
            and not results[paths.index(native_parent)]
            and not raced
        ):
            raced.append(True)
            fs._client.mkdir_many([native_parent], 0o755)
        return results

    monkeypatch.setattr(fs._client, "exists_many", create_parent_after_probe)
    fs.pipe_file(target, b"payload")

    assert raced
    assert fs.cat_file(target) == b"payload"


def test_auto_mkdir_does_not_accept_a_file_created_at_parent(fs, monkeypatch):
    fs.auto_mkdir = True
    parent = f"/prr-mkdir-race-file-{uuid.uuid4().hex}"
    target = f"{parent}/file"
    native_parent = fs._native_path(parent)
    original_exists_many = fs._client.exists_many
    raced = []

    def create_file_after_probe(paths):
        results = original_exists_many(paths)
        if (
            native_parent in paths
            and not results[paths.index(native_parent)]
            and not raced
        ):
            raced.append(True)
            fs._client.write_many([native_parent], [b"parent-file"])
        return results

    monkeypatch.setattr(fs._client, "exists_many", create_file_after_probe)
    with pytest.raises(FileExistsError):
        fs.pipe_file(target, b"payload")

    assert raced
    assert fs.cat_file(parent) == b"parent-file"
    assert not fs.exists(target)


@pytest.mark.parametrize("backend", ["dummy_fs", "nfs_fs"])
def test_auto_mkdir_retries_only_missing_sibling_after_race(
    backend, request, monkeypatch
):
    fs = request.getfixturevalue(backend)
    fs.auto_mkdir = True
    shared = f"/prr-mkdir-race-siblings-{uuid.uuid4().hex}"
    fs.makedirs(shared)
    left = f"{shared}/a"
    right = f"{shared}/b"
    targets = {f"{left}/file": b"left", f"{right}/file": b"right"}
    native_left = fs._native_path(left)
    native_right = fs._native_path(right)
    original_exists_many = fs._client.exists_many
    original_mkdir_many = fs._client.mkdir_many
    raced = []
    mkdir_calls = []

    def create_left_after_probe(paths):
        results = original_exists_many(paths)
        if native_left in paths and not results[paths.index(native_left)] and not raced:
            raced.append(True)
            original_mkdir_many([native_left], 0o755)
        return results

    def fail_raced_batch_once(paths, mode):
        mkdir_calls.append(list(paths))
        if len(mkdir_calls) == 1:
            assert paths == [native_left, native_right]
            # Model mkdir_many stopping at EEXIST for the concurrently created
            # left sibling, leaving the right sibling absent for recovery.
            raise FileExistsError(errno.EEXIST, "already exists", native_left)
        return original_mkdir_many(paths, mode)

    monkeypatch.setattr(fs._client, "exists_many", create_left_after_probe)
    monkeypatch.setattr(fs._client, "mkdir_many", fail_raced_batch_once)
    fs.pipe(targets)

    assert raced
    assert mkdir_calls == [[native_left, native_right], [native_right]]
    assert fs.isdir(left)
    assert fs.isdir(right)
    assert fs.cat_file(f"{left}/file") == b"left"
    assert fs.cat_file(f"{right}/file") == b"right"


@pytest.mark.parametrize("collision", [None, "file", "directory"])
@pytest.mark.parametrize("method", ["mkdir", "makedirs"])
def test_mkdir_validates_leaf_found_in_second_probe(fs, monkeypatch, collision, method):
    fs.mkdir("/parent")
    path = "/parent/child"
    native = fs._native_path(path)
    exists_many = fs._client.exists_many
    lstat_many = fs._client.lstat_many
    stat_many = fs._client.stat_many
    create_dir = getattr(fs, method)
    probes = []
    validations = []

    def observe(present):
        probes.append(present)
        if len(probes) == 1:
            if collision == "file":
                fs._client.write_many([native], [b"concurrent data"])
            elif collision == "directory":
                fs._client.mkdir_many([native], 0o700)

    def observe_existence(paths):
        result = exists_many(paths)
        if native in paths:
            observe(result[paths.index(native)])
        return result

    def observe_lstat(paths):
        attrs, errors = lstat_many(paths)
        if native in paths:
            observe(attrs[paths.index(native)] is not None)
        return attrs, errors

    def record_leaf_metadata(paths, **kwargs):
        if native in paths:
            validations.append(native)
        return stat_many(paths, **kwargs)

    monkeypatch.setattr(fs._client, "exists_many", observe_existence)
    monkeypatch.setattr(fs._client, "lstat_many", observe_lstat)
    with monkeypatch.context() as patch:
        patch.setattr(fs._client, "stat_many", record_leaf_metadata)
        if collision == "file":
            with pytest.raises(FileExistsError) as caught:
                create_dir(path)
            assert caught.value.filename == path
        else:
            create_dir(path)

    if collision == "file":
        assert fs.cat_file(path) == b"concurrent data"
    else:
        assert fs.isdir(path)
        if collision == "directory":
            assert fs.info(path)["mode"] & 0o777 == 0o700
    assert probes == [False, collision is not None]
    assert validations == ([] if collision is None else [native])
