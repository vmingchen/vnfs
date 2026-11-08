# vfsi-c

`vfsi-c` is a C API for the vectorized filesystem interface (`vfsi`). The
functions expose the byte-oriented Unix path API to C consumers (e.g. `git`)
using the Rust `vfsi-sync` contracts and NFS, SMB, and local backend crates.
It shares portable application workflows with `vnfs` where needed.

The public header is checked in at `include/vfsi.h`. A build generates the
same header under Cargo's `OUT_DIR`; it never modifies the source tree. Build
with:

```sh
cargo build -p vfsi-c
```

Run `ci/ports/check-c-headers.sh` for native C99, C11, and C++ header checks,
optional-symbol loader checks, and POSIX field projection on Linux or macOS.
This check uses the checked-in headers and does not build a backend library.

Applications that keep VFSI optional at runtime can include
`include/vfsi_dynamic.h` after the library is installed. Its
`vfsi_dynamic_load()` resolves the common optional port-facing symbols and
checks the ABI once, so each application does not need to maintain its own
`dlsym` declarations. The caller still owns the `dlopen` handle, decides which
optional operations it requires, and keeps the library loaded until all calls
finish. This header does not change the C ABI; consumers that need the helper
should require a `vfsi-c` version that packages it.
On Linux, link applications that use this loader with `-ldl`; this is required
on glibc versions before 2.34, where `dlsym` is provided by a separate library.

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

## Scoped object identity (additive ABI v3)

`vfsi_attrs` is frozen at its ABI-v3 size. Its `fileid` alone is not an object
identity: unrelated filesystems or servers can report the same number. Use the
separate versioned `vfsi_object_id` and these optional exports:

- `vfsi_get_object_id(fs, path, flags, &id)` follows the final symlink by default;
  `VFSI_OBJECT_ID_NOFOLLOW` queries the link itself.
- `vfsi_fget_object_id(fs, fd, &id)` identifies the object pinned by an open C
  descriptor, even after rename or a supported backend reconnect.
- `vfsi_get_object_idv(fs, ops, count, results)` batches paths and retained
  descriptors. Set a path operation's `fd` to -1, or a descriptor operation's
  `path` to NULL. Follow/no-follow changes partition the vector into runs.

All three return `vfsi_result`. Initialize each output header before calling:

```c
struct vfsi_object_id id = {
    .struct_size = sizeof(id), .version = VFSI_OBJECT_ID_VERSION
};
struct vfsi_result result = vfsi_get_object_id(fs, "/file", 0, &id);
if (result.category == VFSI_ERROR_NONE) {
    /* Compare namespace_id, fs_id[0], fs_id[1], and fileid together. */
}
```

`fs_id` preserves the complete 128-bit backend value, high word first.
`namespace_id` is a nonzero comparison token within this process and loaded
library instance, not a persistent identifier. Local roots/mount views share
the local namespace. NFS views using the same configured endpoint share a
namespace regardless of export root, mountpoint or minor version; different
endpoint spellings remain conservatively distinct. Different servers/unknown
backend namespaces cannot alias just because their numeric IDs match. Do not
compare IDs across processes/library reloads, and do not assume durability:
servers can reuse IDs after deletion or reconfiguration.

The query requires both ID components; missing fields return Unsupported, never
fabricated zero values (real zero values remain valid). A vector operation may
also supply optional `vfsi_attrs *attrs` output: attributes and identity then
come from the same GETATTR response. Git uses this to refresh cached stat data
coherently and projects complete namespace/filesystem pairs to collision-free
process-local `st_dev` values, preserving `fileid` as `st_ino`. These projected
device values are not persistent kernel mount/device numbers.

Only successful outputs may be used. All headers/targets are preflighted before
I/O; a preflight rejection leaves payloads untouched. Before dispatch, valid
payloads are cleared; backend failures therefore cannot expose stale IDs.
The output header permits rejecting older/smaller layouts without an overrun
and writing only the supported prefix of larger v1 storage. Future layouts
must use a new version, not change the stride of existing C structures.

The convenience loader exposes these as optional function pointers. Older
ABI-v3 libraries remain compatible; Git falls back to ordinary stat/read paths
when scoped identity is absent or invalid. Build Git against the
SDK header from `vfsi-c` 0.3.11 or newer when using these exports.

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

Example C smoke test (creates its local test root if missing; the backend
itself requires an existing directory):

```sh
cc -I bindings/c/include bindings/c/tests/smoke.c \
   -L target/debug -lvfsi_c -ldl \
   -o /tmp/vfsi_smoke
/tmp/vfsi_smoke /tmp/vfsi_root
```

Run the same smoke test against a guest-accessible SMB share with:

```sh
/tmp/vfsi_smoke --smb 127.0.0.1 vfsi-test
```

Normal builds statically include the packaged ntirpc archive from
`libntirpc-sys`; no system ntirpc or adjacent ntirpc shared library is needed.
The dynamic library still depends on platform libraries such as liburcu and,
when enabled, Kerberos. The build retains an origin-relative RUNPATH to
`target/<profile>/vfsi-libs` for shared ntirpc objects from compatible dependency
overrides; deploy that directory only if your override requires it.
Static-library consumers must link the remaining platform dependencies.
Use `cargo rustc -p vfsi-c --lib -- --print native-static-libs` to inspect them.

### SDK 0.3.15

This patch refreshes the locked backend dependency baseline to include
`vfsi-core 0.1.20`, matching the vnfs 0.0.31 Rust release. Public C interfaces
and ABI v3 are unchanged.

### SDK 0.3.14

This release packages retained directory cursors, inline listing identities,
validated mount discovery, the optional-symbol loader, native POSIX field
projection, and the SDK installer together. The C ABI remains version 3;
existing ABI-v3 clients remain compatible. Applications using the new headers
must continue checking optional function pointers when loading older libraries.
The SDK depends directly on the VFSI backend crates; changing a port's separate
vnfs dependency does not change the C wrapper's exported symbols.

### Building an SDK

Run `python3 build-sdk.py --registry-only --prefix /path/to/sdk` from a
published `vfsi-c` crate checkout. The recipe uses the locked dependency graph,
rejects non-registry dependencies, installs headers and the shared library with
its optional native runtime directory, and writes `versions.json` with actual
dependency versions. `--profile dev` selects a development build. For a workspace
checkout omit `--registry-only`; `--target-dir` can reuse an existing build cache.
Ports can use `VFSI_SDK_SOURCE` to opt into this local recipe while retaining their
published SDK pin for default builds.

The optional identity-listing export returns filesystem-scoped identity with
the attributes from the same directory observation, without a subsequent
GETATTR pass. The optional mount-info callback shares discovery validation with
the mount constructor, so port adapters do not need to parse mount tables.
The native POSIX header contains checked scalar field projection only; callers
still supply scoped identity separately and must not confuse synthetic device
values with kernel device IDs.

## Retained directory cohorts

The optional `vfsi_listdirs_from_cursors` and `vfsi_dir_cursor_free` symbols let
applications carry native child-directory cursors across their own filtered
frontiers. The C ABI stays at v3; dynamic loading leaves absent symbols null.
Each cohort is bounded and fully validated before callbacks. Cursor inputs are
consumed after preflight, and each delivered child is owned by the callback:
retain admitted children for descent and free excluded children. See the header
for cancellation, lifetime, and quota rules.

Applications that also need scoped identities can resolve the optional
`vfsi_listdirs_from_cursors_with_ids` symbol with `vfsi_dir_cursor_free`.
It returns attributes, identity, and retained children from the same READDIR
observation. Copy each borrowed identity before returning; child ownership and
quotas match the cursor-only API. Ordinary cursor listings allocate no identity
storage. If the symbol pair is absent, retain the application's existing
path-based or POSIX fallback.

Streaming callbacks from `vfsi_read_streamv` also run outside backend locks.
Each bounded native read wave completes before delivery; callbacks may query
the same handle and can stop before the next wave. EOF-completion counts and
input-index association are unchanged. Keep the handle and inputs live for the
full streaming call.
