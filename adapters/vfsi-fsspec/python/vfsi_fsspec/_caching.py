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
