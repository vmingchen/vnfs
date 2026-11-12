"""Validated mmap caching shared by direct files and persistent caches."""

import errno

from fsspec.caching import MMapCache


class _Nfs4MMapCache(MMapCache):
    """MMapCache with correct exclusive-end block accounting."""

    def close(self):
        close = getattr(self.cache, "close", None)
        if close is not None:
            close()

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
        ranges = []
        run_start = None
        missing = 0
        for block in requested:
            if block in self.blocks:
                if run_start is not None:
                    ranges.append((run_start, block * self.blocksize))
                    run_start = None
            else:
                missing += 1
                if run_start is None:
                    run_start = block * self.blocksize
        if run_start is not None:
            ranges.append((run_start, min((end_block + 1) * self.blocksize, self.size)))
        self.hit_count += len(requested) - missing

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

        for (range_start, range_end), data in zip(ranges, fetched):
            if len(data) != range_end - range_start:
                raise OSError(errno.EIO, "short read while filling block cache")
            self.cache[range_start:range_end] = data
            first = range_start // self.blocksize
            stop = (range_end - 1) // self.blocksize + 1
            self.blocks.update(range(first, stop))
            self.miss_count += stop - first
            self.total_requested_bytes += range_end - range_start
        return self.cache[start:end]
