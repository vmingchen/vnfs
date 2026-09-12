"""Compatibility fixes for fsspec's persistent block-cache wrapper.

The fixes are deliberately scoped to ``CachingFileSystem`` instances whose
target is nfs4fs. They can be removed after the supported fsspec floor contains
equivalent generation and mmap-boundary fixes.
"""

import errno
import os
import threading
import time

from fsspec.caching import MMapCache
from fsspec.exceptions import BlocksizeMismatchError
from fsspec.implementations.cached import CachingFileSystem

_CACHE_FORMAT_KEY = "nfs4fs_blockcache_format"
_CACHE_FORMAT = 1
_PATCH_MARKER = "_nfs4fs_blockcache_compat"
_INVALID_CACHE_FORMAT = -1


class _Nfs4MMapCache(MMapCache):
    """MMapCache with correct exclusive-end block accounting."""

    def _fetch(self, start, end):
        if start is None:
            start = 0
        if end is None or end > self.size:
            end = self.size
        if start >= self.size or start >= end:
            return b""

        start_block = start // self.blocksize
        end_block = (end - 1) // self.blocksize
        requested = range(start_block, end_block + 1)
        self.hit_count += sum(1 for block in requested if block in self.blocks)
        needed = [block for block in requested if block not in self.blocks]

        groups = []
        for block in needed:
            if groups and block == groups[-1][-1] + 1:
                groups[-1].append(block)
            else:
                groups.append([block])

        ranges = [
            (
                group[0] * self.blocksize,
                min((group[-1] + 1) * self.blocksize, self.size),
            )
            for group in groups
        ]
        multi_fetcher = getattr(self, "multi_fetcher", None)
        if multi_fetcher is not None:
            fetched = list(multi_fetcher(ranges))
            if len(fetched) != len(ranges):
                raise OSError(errno.EIO, "block cache returned the wrong range count")
        else:
            fetched = [
                self.fetcher(range_start, range_end)
                for range_start, range_end in ranges
            ]

        for group, (range_start, range_end), data in zip(groups, ranges, fetched):
            if len(data) != range_end - range_start:
                raise OSError(errno.EIO, "short read while filling block cache")
            self.cache[range_start:range_end] = data
            self.blocks.update(group)
            self.miss_count += len(group)
            self.total_requested_bytes += range_end - range_start
        return self.cache[start:end]


def _cache_path(cache_fs, path):
    path = cache_fs._strip_protocol(path)
    return cache_fs.fs._strip_protocol(path)


def _save_writable_metadata(cache_fs):
    """Replace, rather than merge, the writable cache metadata generation."""
    metadata = cache_fs._metadata
    serializable = {}
    for path, detail in metadata.cached_files[-1].items():
        stored = detail.copy()
        if isinstance(stored.get("blocks"), set):
            stored["blocks"] = list(stored["blocks"])
        serializable[path] = stored
    metadata._save(serializable, os.path.join(cache_fs.storage[-1], "cache"))
    cache_fs.last_cache = time.time()
    cache_fs._cache_size = None


def _backing_path(cache_fs, detail):
    storage = os.path.realpath(cache_fs.storage[-1])
    candidate = os.path.join(storage, detail["fn"])
    if (
        os.path.islink(candidate)
        or os.path.commonpath([storage, os.path.realpath(candidate)]) != storage
    ):
        raise ValueError("unsafe path in fsspec block-cache metadata")
    return candidate


def _reset_stale_generation(cache_fs, path):
    raw = cache_fs._metadata.check_file(path, None)
    if not raw:
        return
    raw_detail, _ = raw
    writable_detail = cache_fs._metadata.cached_files[-1].get(path)
    if writable_detail is None or writable_detail.get("fn") != raw_detail.get("fn"):
        return

    backing_file = _backing_path(cache_fs, writable_detail)
    valid = cache_fs._check_file(path)
    compatible = writable_detail.get(
        _CACHE_FORMAT_KEY
    ) == _CACHE_FORMAT and os.path.getsize(backing_file) == writable_detail.get("size")
    if valid and compatible:
        return

    info = cache_fs.fs.info(path)
    replacement = writable_detail.copy()
    replacement.update(
        {
            "blocks": set(),
            "size": info["size"],
            "time": time.time(),
            "uid": cache_fs.fs._ukey_from_info(info),
            _CACHE_FORMAT_KEY: _CACHE_FORMAT,
        }
    )
    replacement.pop("blocksize", None)
    cache_fs._metadata.cached_files[-1][path] = replacement
    _save_writable_metadata(cache_fs)
    with open(backing_file, "wb") as file:
        file.truncate(info["size"])


def _mark_compatible_cache(cache_fs, path, file):
    cache = getattr(file, "cache", None)
    if isinstance(cache, MMapCache) and not isinstance(cache, _Nfs4MMapCache):
        cache.__class__ = _Nfs4MMapCache
        block_count = (cache.size + cache.blocksize - 1) // cache.blocksize
        cache.blocks.intersection_update(range(block_count))

    detail = cache_fs._metadata.cached_files[-1].get(path)
    if detail is not None and detail.get(_CACHE_FORMAT_KEY) != _CACHE_FORMAT:
        detail[_CACHE_FORMAT_KEY] = _CACHE_FORMAT
        _save_writable_metadata(cache_fs)


def _cache_lock(cache_fs):
    attributes = object.__getattribute__(cache_fs, "__dict__")
    lock = attributes.get("_nfs4fs_cache_lock")
    if lock is None:
        lock = threading.RLock()
        attributes["_nfs4fs_cache_lock"] = lock
    return lock


def _invalidate_registered_paths(cache_fs, paths, subtrees=False):
    """Invalidate future opens without disrupting already-open mmap handles."""
    with _cache_lock(cache_fs):
        cache_fs._check_cache()
        writable = cache_fs._metadata.cached_files[-1]

        def affected(candidate):
            for path in paths:
                if candidate == path:
                    return True
                if subtrees and (
                    path == "/" or candidate.startswith(path.rstrip("/") + "/")
                ):
                    return True
            return False

        changed = False
        for path, detail in writable.items():
            if (
                affected(path)
                and detail.get(_CACHE_FORMAT_KEY) != _INVALID_CACHE_FORMAT
            ):
                detail[_CACHE_FORMAT_KEY] = _INVALID_CACHE_FORMAT
                changed = True
        if changed:
            _save_writable_metadata(cache_fs)


def _register_cache(cache_fs):
    cache_fs.fs._register_persistent_cache(cache_fs, _invalidate_registered_paths)


def _enter_individually(open_files):
    entered = []
    try:
        for open_file in open_files:
            entered.append(open_file.__enter__())
    except BaseException:
        for open_file in reversed(open_files[: len(entered)]):
            open_file.__exit__(None, None, None)
        raise
    return entered


def _delegate_open_many(cache_fs, open_files):
    fs = cache_fs.fs
    while True:
        open_many = getattr(fs, "open_many", None)
        if callable(open_many):
            return open_many(open_files)
        nested = getattr(fs, "fs", None)
        if nested is None:
            return _enter_individually(open_files)
        fs = nested


def _delegate_commit_many(cache_fs, files):
    fs = cache_fs.fs
    while True:
        commit_many = getattr(fs, "commit_many", None)
        if callable(commit_many):
            return commit_many(files)
        nested = getattr(fs, "fs", None)
        if nested is None:
            return None
        fs = nested


def _is_plain_read(open_file):
    mode = open_file.mode.replace("t", "").replace("b", "")
    return mode == "r" and open_file.compression is None


def _raw_cache_records(cache_fs, paths):
    cache_fs._mkcache()
    cache_fs._check_cache()
    records = []
    info_indices = []
    now = time.time()
    for index, path in enumerate(paths):
        raw = cache_fs._metadata.check_file(path, None)
        detail, filename = raw if raw else (None, None)
        compatible = False
        expired = False
        if detail is not None:
            try:
                compatible = detail.get(
                    _CACHE_FORMAT_KEY
                ) == _CACHE_FORMAT and os.path.getsize(filename) == detail.get("size")
            except OSError:
                compatible = False
            expired = bool(
                cache_fs.expiry and now - detail.get("time", 0) > cache_fs.expiry
            )
        needs_info = (
            detail is None or not compatible or expired or bool(cache_fs.check_files)
        )
        if needs_info:
            info_indices.append(index)
        records.append(
            {
                "detail": detail,
                "filename": filename,
                "compatible": compatible,
                "expired": expired,
                "info": None,
            }
        )
    return records, info_indices


def _prepare_cache_records(cache_fs, paths):
    records, info_indices = _raw_cache_records(cache_fs, paths)
    if info_indices:
        infos = cache_fs.fs._info_many([paths[index] for index in info_indices])
        for index, info in zip(info_indices, infos):
            records[index]["info"] = info

    replacements = []
    writable_cache = cache_fs._metadata.cached_files[-1]
    now = time.time()
    for path, record in zip(paths, records):
        detail = record["detail"]
        valid = detail is not None and record["compatible"] and not record["expired"]
        if valid and cache_fs.check_files:
            valid = detail.get("uid") == cache_fs.fs._ukey_from_info(record["info"])

        if valid:
            if detail["blocks"] is True:
                continue
            writable = writable_cache.get(path)
            if writable is None or writable.get("fn") != detail.get("fn"):
                # fsspec does not support filling a partial cache entry from a
                # read-only cache location. Fall back to its scalar behavior.
                return None
            if not isinstance(writable.get("blocks"), set):
                writable["blocks"] = set(writable.get("blocks", ()))
            record["detail"] = writable
            record["filename"] = _backing_path(cache_fs, writable)
            continue

        info = record["info"]
        existing = writable_cache.get(path)
        if existing is not None:
            replacement = existing.copy()
        else:
            replacement = {
                "original": path,
                "fn": cache_fs._mapper(path),
            }
        replacement.update(
            {
                "blocks": set(),
                "size": info["size"],
                "time": now,
                "uid": cache_fs.fs._ukey_from_info(info),
                _CACHE_FORMAT_KEY: _CACHE_FORMAT,
            }
        )
        replacement.pop("blocksize", None)
        writable_cache[path] = replacement
        record["detail"] = replacement
        record["filename"] = _backing_path(cache_fs, replacement)
        replacements.append(record)

    if replacements:
        # Persist the empty generation before touching a reused sparse file,
        # so a crash cannot expose old blocks as belonging to the new uid.
        _save_writable_metadata(cache_fs)
        for record in replacements:
            with open(record["filename"], "wb") as file:
                file.truncate(record["detail"]["size"])
    return records


def _open_many_with_cache(cache_fs, open_files, target_type):
    if not isinstance(getattr(cache_fs, "fs", None), target_type):
        return _delegate_open_many(cache_fs, open_files)
    _register_cache(cache_fs)
    if not all(_is_plain_read(open_file) for open_file in open_files):
        if all("r" in open_file.mode for open_file in open_files):
            return _enter_individually(open_files)
        return _delegate_open_many(cache_fs, open_files)

    paths = [_cache_path(cache_fs, open_file.path) for open_file in open_files]
    records = _prepare_cache_records(cache_fs, paths)
    if records is None:
        return _enter_individually(open_files)

    results = [None] * len(paths)
    remote_indices = []
    for index, record in enumerate(records):
        if record["detail"]["blocks"] is True:
            results[index] = open(record["filename"], "rb")
        else:
            remote_indices.append(index)

    remote_files = []
    try:
        if remote_indices:
            remote_open_files = [open_files[index] for index in remote_indices]
            remote_files = cache_fs.fs.open_many(
                remote_open_files,
                cache_type="none",
                sizes=[records[index]["detail"]["size"] for index in remote_indices],
            )
            for index, file in zip(remote_indices, remote_files):
                detail = records[index]["detail"]
                old_blocksize = detail.get("blocksize")
                if old_blocksize is not None and old_blocksize != file.blocksize:
                    raise BlocksizeMismatchError(
                        "Cached file must be reopened with same block size as "
                        f"original (old: {old_blocksize}, new {file.blocksize})"
                    )
                detail["blocksize"] = file.blocksize
                file.cache = _Nfs4MMapCache(
                    file.blocksize,
                    file._fetch_range,
                    file.size,
                    records[index]["filename"],
                    detail["blocks"],
                )
                file.cache_type = "mmap"
                close = file.close
                file.close = lambda file=file, close=close: cache_fs.close_and_update(
                    file, close
                )
                results[index] = file

        cache_fs.save_cache()
        for open_file, file in zip(open_files, results):
            open_file.fobjects = [file]
        return results
    except BaseException:
        for file in results:
            if file is not None:
                try:
                    file.close()
                except BaseException:
                    pass
        for file in remote_files:
            if file not in results:
                try:
                    file.close()
                except BaseException:
                    pass
        raise


def install_fsspec_blockcache_compat(target_type):
    """Install nfs4fs-only fixes around fsspec's persistent block cache."""
    if getattr(CachingFileSystem, _PATCH_MARKER, False):
        return
    original_init = CachingFileSystem.__init__
    original_open = CachingFileSystem._open
    original_close_and_update = CachingFileSystem.close_and_update

    def init_with_compat(cache_fs, *args, **kwargs):
        original_init(cache_fs, *args, **kwargs)
        if type(cache_fs) is CachingFileSystem and isinstance(
            getattr(cache_fs, "fs", None), target_type
        ):
            _register_cache(cache_fs)

    def open_with_compat(cache_fs, path, *args, **kwargs):
        if not isinstance(getattr(cache_fs, "fs", None), target_type):
            return original_open(cache_fs, path, *args, **kwargs)
        _register_cache(cache_fs)
        mode = kwargs.get("mode", args[0] if args else "rb")
        if mode.replace("t", "").replace("b", "") != "r":
            return original_open(cache_fs, path, *args, **kwargs)
        with _cache_lock(cache_fs):
            normalized = _cache_path(cache_fs, path)
            _reset_stale_generation(cache_fs, normalized)
            file = original_open(cache_fs, path, *args, **kwargs)
            _mark_compatible_cache(cache_fs, normalized, file)
            return file

    def close_and_update_with_compat(cache_fs, file, close):
        if not isinstance(getattr(cache_fs, "fs", None), target_type):
            return original_close_and_update(cache_fs, file, close)
        with _cache_lock(cache_fs):
            return original_close_and_update(cache_fs, file, close)

    def open_many_with_compat(cache_fs, open_files):
        if not isinstance(getattr(cache_fs, "fs", None), target_type):
            return _delegate_open_many(cache_fs, open_files)
        with _cache_lock(cache_fs):
            return _open_many_with_cache(cache_fs, open_files, target_type)

    CachingFileSystem.__init__ = init_with_compat
    CachingFileSystem._open = open_with_compat
    CachingFileSystem.close_and_update = close_and_update_with_compat
    CachingFileSystem.open_many = open_many_with_compat
    CachingFileSystem.commit_many = _delegate_commit_many
    setattr(CachingFileSystem, _PATCH_MARKER, True)
