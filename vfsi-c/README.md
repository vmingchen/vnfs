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

ABI v3 retains every ABI-v2 symbol and adds genuinely vectorized `vfsi_openv`,
`vfsi_closev`, `vfsi_statv`, `vfsi_setattrv`, `vfsi_preadv`, `vfsi_pwritev`,
`vfsi_mkdirv`, `vfsi_removev`, `vfsi_renamev`, and `vfsi_copyv` calls. They
return a uniform `vfsi_result`: the overall result's `index` is the completed
count on success or the failing element on error, `category` is
protocol-independent, `err_no` retains the backend status, and `message`
carries transport diagnostics. Callers also provide one `vfsi_result` per
element. Successful batches mark every element successful. A request rejected
before submission marks the failing element and leaves the rest not attempted;
after a submitted concurrent batch fails, non-failing elements are explicitly
marked indeterminate because they may already have completed.

For large repositories, `vfsi_read_streamv()` reads several paths in bounded
chunks. `chunk_size` limits each read and `memory_limit` bounds the aggregate
bytes requested per backend batch. Returning `false` from its callback stops
successfully, providing cancellation and backpressure without buffering whole
files. The streaming callback must not reenter the same filesystem handle.

For an NFS mount whose source is `server:/exports/repos` and whose local
mountpoint is `/mnt/repos`, use:

```c
vfsi_nfs_open_mount_export("server", "/exports/repos", "/mnt/repos", &fs);
```

The older `vfsi_nfs_open_mount()` remains available and maps the local
mountpoint to the NFSv4 pseudo-root (`/`).

`vfsi_nfs_open()` negotiates NFSv4.2 and falls back to v4.1.
`vfsi_nfs_open_minor()` pins either minor version. Use
`vfsi_nfs_minorversion()` and `vfsi_capabilities()` to inspect the resulting
connection; `vfsi_copy()` uses server COPY when advertised and otherwise
falls back to client-side copying.

For a Samba or other SMB2/3 share, use:

```c
vfsi_smb_open("server", "share", "username", "password", "domain", &fs);
```

The username, password, and domain may be empty strings for guest access.
`vfsi_smb_dialect()` reports the negotiated dialect (`0x0202` through
`0x0311`). Small path I/O and metadata operations use related SMB compounds;
independent vector elements are multiplexed concurrently while dependent
mutations remain ordered. Large transfers honor the negotiated read/write
limits. `vfsi_copy()` uses
SMB server-side copy when supported and permanently switches that connection
to the client-side fallback if the server rejects the operation.

SMB paths must be valid UTF-8. Unix permission modes and symlink/hard-link
operations require SMB POSIX extensions, which this backend does not yet
expose; chmod and link operations therefore report `ENOTSUP`.
Use `vfsi_capabilities()` and the granular `VFSI_CAP_*` bits to distinguish
these optional semantics from portable file, directory, and I/O operations.

Example C smoke test:

```sh
cc -I vfsi-c/include vfsi-c/tests/smoke.c \
   -L target/debug -lvfsi_c \
   -o /tmp/vfsi_smoke
/tmp/vfsi_smoke /tmp/vfsi_root
```

Run the same smoke test against a guest-accessible SMB share with:

```sh
/tmp/vfsi_smoke --smb 127.0.0.1 vfsi-test
```

The dynamic build places its libntirpc runtime libraries in
`target/<profile>/vfsi-libs` and gives `libvfsi_c.so` an origin-relative
RUNPATH to that directory. Deploy the shared library together with the
adjacent `vfsi-libs` directory; no Cargo-specific `LD_LIBRARY_PATH` is needed.
Static-library consumers must link libntirpc and its platform dependencies
themselves.
