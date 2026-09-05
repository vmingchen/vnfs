# vfsi-c

`vfsi-c` is a C API for the vectorized filesystem interface (`vfsi`). The
functions expose the byte-oriented Unix path API to C consumers (e.g. `git`)
while keeping the implementation in the Rust `vnfs` crate.

The public header is checked in at `include/vfsi.h`. A build generates the
same header under Cargo's `OUT_DIR`; it never modifies the source tree. Build
with:

```sh
cargo build -p vfsi-c
```

Consumers that load the library dynamically must call `vfsi_abi_version()`
and require `VFSI_ABI_VERSION` before resolving or invoking the rest of the
API. Attribute structures also carry their size and ABI version.

For an NFS mount whose source is `server:/exports/repos` and whose local
mountpoint is `/mnt/repos`, use:

```c
vfsi_nfs_open_mount_export("server", "/exports/repos", "/mnt/repos", &fs);
```

The older `vfsi_nfs_open_mount()` remains available and maps the local
mountpoint to the NFSv4 pseudo-root (`/`).

Example C smoke test:

```sh
cc -I vfsi-c/include vfsi-c/tests/smoke.c \
   -L target/debug -lvfsi_c \
   -o /tmp/vfsi_smoke
/tmp/vfsi_smoke /tmp/vfsi_root
```

The dynamic build places its libntirpc runtime libraries in
`target/<profile>/vfsi-libs` and gives `libvfsi_c.so` an origin-relative
RUNPATH to that directory. Deploy the shared library together with the
adjacent `vfsi-libs` directory; no Cargo-specific `LD_LIBRARY_PATH` is needed.
Static-library consumers must link libntirpc and its platform dependencies
themselves.
