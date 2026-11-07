# Parent/name directory cohort and page-budget sweep

Keep the current production defaults. Wider cohorts did not consistently
reduce RPC attempts on the existing server; some triggered safe resource
repacking even after warmup. A larger requested page cap did not consistently
increase the effective page size under negotiated limits.

The sweep uses independent clients for every setting, C stat attributes,
cohort widths 8/16/24/31/32, reply caps 4/16/32/64 KiB, NFSv4.1/v4.2, and
the ordinary negotiated operation limit or an imposed client cap of 33.
The server configuration is never changed. Each setting has three warm scans
and three measured scans. All 480 measured complete traversals have identical
names/types. Transport failures or incomplete scans fail the test.

The directory-dense tree has 128 directories with eight files each (1,152
entries). The file-dense tree has four such directories and 4,096 root files
(4,132 entries). Connection/root resolution and fixture creation are excluded.
Clients are isolated so one shape cannot lower another shape’s adaptive limit.

RPC ranges below include any safely retried RESOURCE/TOO_MANY_OPS attempts.
These attempts remain in the raw samples; they are not discarded as outliers.
The larger shapes are not always stationary after warmup. Therefore their
timings are not used to justify a default change.

| Fixture | Client op cap | Width | Reply cap KiB | RPCs v4.1 | RPCs v4.2 |
|---|---:|---:|---:|---:|---:|
| directory dense | negotiated | 24 | 4 | 11 | 11 |
| directory dense | negotiated | 24 | 32 | 7 | 7 |
| directory dense | negotiated | 31 | 32 | 10–15 | 10–15 |
| directory dense | negotiated | 32 | 32 | 9–14 | 9–14 |
| directory dense | negotiated | 24 | 64 | 7 | 7 |
| directory dense | 33 | 24 | 4 | 21 | 21 |
| directory dense | 33 | 24 | 32 | 17 | 17 |
| directory dense | 33 | 31 | 32 | 18 | 18 |
| directory dense | 33 | 32 | 32 | 17 | 17 |
| directory dense | 33 | 24 | 64 | 17 | 17 |
| file dense | negotiated | 24 | 4 | 138 | 138 |
| file dense | negotiated | 24 | 32 | 18 | 18 |
| file dense | negotiated | 31 | 32 | 18 | 18 |
| file dense | negotiated | 32 | 32 | 18 | 18 |
| file dense | negotiated | 24 | 64 | 18 | 18 |
| file dense | 33 | 24 | 4 | 138 | 138 |
| file dense | 33 | 24 | 32 | 18 | 18 |
| file dense | 33 | 31 | 32 | 18 | 18 |
| file dense | 33 | 32 | 32 | 18 | 18 |
| file dense | 33 | 24 | 64 | 18 | 18 |

The raw data include all 160 groups, not only these selected comparisons.
Counters are per-client telemetry, not packet captures. Request bytes are
encoded COMPOUND arguments; returned bytes count raw entry attributes only.
The unoptimized native-loopback timings are diagnostic, not release benchmarks.

```sh
VFSI_NFS_SERVER=127.0.0.1 VFSI_NFS_TEST_MOUNT=/mnt/nfs VFSI_NFS_MINOR=2 \
  cargo test -p vfsi-nfs --lib --all-features \
    nfs::directory_benchmark::compare_parent_name_cohorts_and_page_budgets \
    -- --ignored --exact --nocapture
```

Repeat with minor 1. The mount must correspond to the configured export root.
Only exclusive, task-owned fixture directories are created and removed.

[All 480 samples](../benchmarks/nfs-directory-handles/cohorts.json).
The earlier returned-filehandle experiment and the published rsync baseline
remain unchanged.
