"""Compatibility fixes for fsspec's persistent block-cache wrapper.

The fixes are deliberately scoped to ``CachingFileSystem`` instances backed
by the VFSI adapter engine. They can be removed after the supported fsspec
floor contains equivalent generation and mmap-boundary fixes.
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
        if multi_fetcher is not None and len(ranges) > 1:
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
    if isinstance(cache, MMapCache):
        # LocalFileSystem establishes its descriptor during open(), so an
        # already-open cache handle remains usable after rename or unlink.
        # VfsiFile normally opens lazily for vectorized whole-file fast paths;
        # persistent blockcache handles need the local descriptor lifetime.
        raw = getattr(file, "_raw", None)
        if raw is not None:
            raw._ensure_open()
    if isinstance(cache, MMapCache) and not isinstance(cache, _Nfs4MMapCache):
        cache.__class__ = _Nfs4MMapCache
        block_count = (cache.size + cache.blocksize - 1) // cache.blocksize
        cache.blocks.intersection_update(range(block_count))

    detail = cache_fs._metadata.cached_files[-1].get(path)
    if detail is not None:
        changed = False
        if detail.get(_CACHE_FORMAT_KEY) != _CACHE_FORMAT:
            detail[_CACHE_FORMAT_KEY] = _CACHE_FORMAT
            changed = True
        # fsspec 2024.12 does not add size to scalar blockcache metadata.
        # Persist it so compatibility checks do not discard a valid partial
        # generation, its block size, or its populated block set on reopen.
        file_size = getattr(file, "size", None)
        if file_size is not None and detail.get("size") != file_size:
            detail["size"] = file_size
            changed = True
        if isinstance(cache, MMapCache) and detail.get("blocksize") != file.blocksize:
            detail["blocksize"] = file.blocksize
            changed = True
        if changed:
            _save_writable_metadata(cache_fs)


def _close_and_update_complete_blocks(cache_fs, file, close):
    """Persist a partial generation using inclusive final-block accounting."""
    if file.closed:
        return
    path = cache_fs._strip_protocol(file.path)
    detail = cache_fs._metadata.cached_files[-1][path]
    cache = getattr(file, "cache", None)
    blocks = getattr(cache, "blocks", detail["blocks"])
    detail["blocks"] = blocks
    if blocks is not True:
        block_count = (file.size + file.blocksize - 1) // file.blocksize
        if all(block in blocks for block in range(block_count)):
            detail["blocks"] = True
    try:
        # Preserve nfs4fs's size, format, and block-size metadata instead of
        # fsspec's narrower merge-on-save fields.
        _save_writable_metadata(cache_fs)
    except (NameError, OSError):
        # Match fsspec's close behavior during interpreter shutdown and when
        # best-effort metadata persistence is unavailable.
        pass
    close()
    file.closed = True


def _reuse_generation_blocksize(cache_fs, path, args, kwargs):
    """Keep a partial generation's block size across scalar reopens.

    LocalFileSystem ignores a later ``block_size`` request because its opener
    retains the same native block size. Mirror that public behavior while
    preserving fsspec's requirement that a partial persistent-cache
    generation has exactly one block size.
    """
    raw = cache_fs._metadata.check_file(path, None)
    if not raw:
        return args, kwargs
    blocksize = raw[0].get("blocksize")
    if blocksize is None:
        return args, kwargs
    args = list(args)
    kwargs = dict(kwargs)
    if len(args) >= 2:
        args[1] = blocksize
    else:
        kwargs["block_size"] = blocksize
    return tuple(args), kwargs


def _cache_lock(cache_fs):
    fs_attributes = object.__getattribute__(cache_fs.fs, "__dict__")
    registry_lock = fs_attributes.get("_nfs4fs_cache_locks_lock")
    if registry_lock is None:
        registry_lock = threading.RLock()
        fs_attributes["_nfs4fs_cache_locks_lock"] = registry_lock
    key = tuple(os.path.realpath(storage) for storage in cache_fs.storage)
    with registry_lock:
        locks = fs_attributes.setdefault("_nfs4fs_cache_locks", {})
        return locks.setdefault(key, threading.RLock())


def _invalidate_registered_paths(cache_fs, paths, subtrees=False):
    """Invalidate future opens without disrupting already-open mmap handles."""
    with _cache_lock(cache_fs):
        # Multiple CachingFileSystem wrappers may share one writable storage
        # directory. cache_check=0 intentionally disables fsspec's automatic
        # reload, so refresh here before replacing shared generation metadata.
        cache_fs.load_cache()
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


def _pop_writable_cache_file(cache_fs, path):
    """Remove one writable generation without fsspec's merge-on-save."""
    normalized = _cache_path(cache_fs, path)
    with _cache_lock(cache_fs):
        cache_fs._check_cache()
        writable = cache_fs._metadata.cached_files[-1]
        detail = writable.pop(normalized, None)
        if detail is None:
            if cache_fs._metadata.check_file(normalized, None):
                raise PermissionError(
                    "Can only delete cached file in last, writable cache location"
                )
            return
        filename = _backing_path(cache_fs, detail)
        # CacheMetadata.save() merges entries from the previous on-disk file,
        # which resurrects a deletion. Replace the writable generation first.
        _save_writable_metadata(cache_fs)
        try:
            os.remove(filename)
        except FileNotFoundError:
            pass
        cache_fs._cache_size = None


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
            # VfsiFileSystem.open_many() has one block size per vector call.
            # Reuse each partial generation's established size and retain one
            # batched OPEN for every compatible cohort.
            cohorts = {}
            for index in remote_indices:
                blocksize = records[index]["detail"].get("blocksize")
                if blocksize is None:
                    blocksize = cache_fs.fs.block_size
                cohorts.setdefault(blocksize, []).append(index)
            indexed_files = {}
            for blocksize, indices in cohorts.items():
                opened = cache_fs.fs.open_many(
                    [open_files[index] for index in indices],
                    block_size=blocksize,
                    cache_type="none",
                    sizes=[records[index]["detail"]["size"] for index in indices],
                )
                remote_files.extend(opened)
                indexed_files.update(zip(indices, opened))

            for index in remote_indices:
                file = indexed_files[index]
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

        # CacheMetadata.save() merges only blocks, time, and uid from an
        # existing record. Replace the writable record so the block size just
        # established by vectorized open survives the first save.
        _save_writable_metadata(cache_fs)
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
    original_pop_from_cache = CachingFileSystem.pop_from_cache

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
            args, kwargs = _reuse_generation_blocksize(
                cache_fs, normalized, args, kwargs
            )
            file = original_open(cache_fs, path, *args, **kwargs)
            _mark_compatible_cache(cache_fs, normalized, file)
            return file

    def close_and_update_with_compat(cache_fs, file, close):
        if not isinstance(getattr(cache_fs, "fs", None), target_type):
            return original_close_and_update(cache_fs, file, close)
        with _cache_lock(cache_fs):
            return _close_and_update_complete_blocks(cache_fs, file, close)

    def open_many_with_compat(cache_fs, open_files):
        if not isinstance(getattr(cache_fs, "fs", None), target_type):
            return _delegate_open_many(cache_fs, open_files)
        with _cache_lock(cache_fs):
            return _open_many_with_cache(cache_fs, open_files, target_type)

    def pop_from_cache_with_compat(cache_fs, path):
        if not isinstance(getattr(cache_fs, "fs", None), target_type):
            return original_pop_from_cache(cache_fs, path)
        return _pop_writable_cache_file(cache_fs, path)

    CachingFileSystem.__init__ = init_with_compat
    CachingFileSystem._open = open_with_compat
    CachingFileSystem.close_and_update = close_and_update_with_compat
    CachingFileSystem.open_many = open_many_with_compat
    CachingFileSystem.commit_many = _delegate_commit_many
    CachingFileSystem.pop_from_cache = pop_from_cache_with_compat
    setattr(CachingFileSystem, _PATCH_MARKER, True)
