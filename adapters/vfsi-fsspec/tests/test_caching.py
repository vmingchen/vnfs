"""Bounded cache planning retains only missing-range endpoints."""

import pytest
from vfsi_fsspec._caching import _Nfs4MMapCache


class CountingSet(set):
    def __init__(self, values):
        super().__init__(values)
        self.lookups = 0

    def __contains__(self, value):
        self.lookups += 1
        return super().__contains__(value)


@pytest.mark.parametrize("end", [5, 8])
@pytest.mark.parametrize("multi", [False, True])
def test_mmap_fetch_checks_each_block_once_and_preserves_gaps(tmp_path, end, multi):
    payload = b"abcdefgh"
    backing = tmp_path / "blocks"
    backing.write_bytes(payload)
    blocks = CountingSet({0, 2})
    requests = []

    def fetch(start, stop):
        requests.append((start, stop))
        return payload[start:stop]

    def fetch_many(ranges):
        return [fetch(start, stop) for start, stop in ranges]

    cache = _Nfs4MMapCache(
        2,
        fetch,
        len(payload),
        str(backing),
        blocks,
        multi_fetcher=fetch_many if multi else None,
    )
    try:
        assert cache._fetch(0, end) == payload[:end]
        requested = (end + 1) // 2
        assert blocks.lookups == requested
        assert cache.hit_count == 2
        expected_ranges = [(2, 4)] if end == 5 else [(2, 4), (6, 8)]
        assert requests == expected_ranges
        assert cache.miss_count == len(expected_ranges)
        assert cache.total_requested_bytes == 2 * len(expected_ranges)
        assert blocks == set(range(requested))
        assert cache._fetch(0, end) == payload[:end]
        assert requests == expected_ranges
        assert blocks.lookups == 2 * requested
    finally:
        cache.close()


def test_mmap_missing_run_preserves_partial_final_block_and_counters(tmp_path):
    payload = b"abcdefgh"
    backing = tmp_path / "tail"
    backing.write_bytes(payload)
    requests = []

    def fetch(start, end):
        requests.append((start, end))
        return payload[start:end]

    cache = _Nfs4MMapCache(3, fetch, len(payload), str(backing), {0})
    try:
        assert cache._fetch(1, None) == payload[1:]
        assert requests == [(3, 8)]
        assert cache.blocks == {0, 1, 2}
        assert cache.hit_count == 1 and cache.miss_count == 2
        assert cache.total_requested_bytes == 5
    finally:
        cache.close()
