# libntirpc-sys

Low-level bindings for the [libntirpc](https://github.com/nfs-ganesha/ntirpc)
library.

Some documentation for the library can be found in the Linux man pages
[rpc(3)](https://linux.die.net/man/3/rpc) and
[xdr(3)](https://linux.die.net/man/3/xdr). However, the doc has some
inconsistency with the library, so we should use the doc as a reference not as a
definitive guide.

Its implementation is adapted from the
[libtirpc-sys](https://crates.io/crates/libtirpc-sys) library.

## Examples

The [examples](examples/) directory contains a small "hello world" RPC
client-server pair built directly on the raw bindings. The client issues a
single synchronous call:

```rust
use libntirpc_sys::*;

// Send `name`, receive the greeting (`wrap_string` adapts `xdr_wrapstring`
// to the generic `xdrproc_t` signature; see examples/hello_client.rs).
let stat = unsafe {
    rpc_call(
        host.as_ptr(), HELLO_PROG, HELLO_VERS, HELLO_PROC,
        Some(wrap_string),                       // encode the argument
        &mut name as *mut _ as *const c_void,
        Some(wrap_string),                       // decode the result
        &mut greeting as *mut _ as *mut c_void,
        c"udp".as_ptr(),                         // transport
    )
};
assert!(stat == clnt_stat_RPC_SUCCESS);
```

The pair needs the `rpcbind` daemon, which is used to register the server's
program number and for the client to look it up:

```sh
sudo apt install rpcbind
```

Make sure it is running (restart it if the server fails to register):

```sh
sudo systemctl restart rpcbind
```

Run the server in one terminal:

```sh
cargo run --example hello_server
```

It should print `hello server: prog=0x30000001 vers=1 on udp` and register
with rpcbind (check with `rpcinfo -p`). Then, in another terminal, call it:

```sh
cargo run --example hello_client -- Alice 127.0.0.1
```

which prints `Hello, Alice!`. Both arguments are optional (`name` defaults to
`world`, `host` to `127.0.0.1`).

## Dependencies

Normal builds discover the administrator-provided `libntirpc` through
`pkg-config` and generate Rust declarations from its installed headers. The
build script does not access the network or download native source. The
currently supported native target is Linux. `build.rs` accepts `libntirpc` 6.3
or newer, but that is only a lower bound; see the ABI notes below for the
releases actually validated.

| Dependency | Package (Ubuntu) | Purpose |
| --- | --- | --- |
| libntirpc | `libntirpc-dev` | RPC implementation and headers (6.3+, validated on 6.3 and 15.x) |
| userspace RCU | `liburcu-dev` | linker name for libntirpc's `urcu-bp` dependency |
| pkg-config | `pkg-config` | locate the installed library and headers |
| C toolchain | `build-essential` | compile the `auth_helpers.c` ABI shims |
| clang | `clang` | provide the headers used by bindgen to generate bindings |
| GSSAPI headers | `libkrb5-dev` | only for the optional `rpcsec-gss` feature |

Install them on Ubuntu with:

```sh
sudo apt install clang libclang-dev pkg-config libntirpc-dev liburcu-dev
```

Add `libkrb5-dev` only when building the `rpcsec-gss` feature:

```sh
sudo apt install libkrb5-dev
```

The `libntirpc` shared object is linked against `liburcu-bp` and, when the
distribution enables it, `libgssapi_krb5`. The runtime `libntirpc6.3` package
supplies runtime dependencies, but builds that link a Rust binary still need
`liburcu-dev` for the unversioned linker name.

docs.rs uses checked-in declarations and therefore does not require native
packages. Those declarations are only a documentation input; normal builds
always bind to the locally installed headers.

## Supported libntirpc versions and ABI notes

The crate links the administrator-provided `libntirpc` and never vendors
native source, so compatibility is not a simple version range. `build.rs`
accepts 6.3 or newer, but the RPC helpers depend on a copied private
`rpc_dplx_rec` layout and on `SVCXPRT` having been built with `INET6`; neither
is exposed by the installed headers (`sizeof` in the C shim is compared only
with bindgen, not with the linked library). The releases validated so far are:

- Ubuntu 24.04's `libntirpc` 6.3: build, unit tests and the RPCSEC_GSS runtime
  integration job (client and server).
- Upstream v15.3: ABI/compile tests and the scheduled client-side RPCSEC_GSS
  runtime job.

Any other 6.3+ release is accepted but not guaranteed to match the private
layout or to have been built with `INET6`. Re-run the RPCSEC_GSS integration
test against a new library before enabling the feature in production.

Several upstream ABI changes did not bump the reported version, which is why
`build.rs` verifies what it can:

- The private `struct rpc_dplx_rec` gained an `rdma_call_expires` member
  together with the exported `clnt_tli_ncreate_opt` symbol (upstream v9.15),
  but the reported version stayed `7.2`. `build.rs` therefore probes the
  linked library's symbols (`nm`, overridable with `NM`) instead of trusting
  the version string. If the probe cannot run, set
  `LIBNTIRPC_RPC_DPLX_RDMA_EXPIRES=0` or `=1` explicitly; an ambiguous
  version with no probe fails the build instead of guessing.
- The `clnt_req` free callback changed arity at upstream v15.0. The crate
  exposes this as the `DEP_NTIRPC_LEGACY_FREE_CB` build-script variable for
  dependent crates.
- `SVCXPRT` is compiled with `_GNU_SOURCE` and `INET6`, matching the
  supported Linux packages. A unit test asserts that the C shims and the
  bindgen output agree on `sizeof(SVCXPRT)`, so a flag divergence fails the
  build rather than corrupting the private layout.
- The `rpcsec-gss` feature patches a private `rpc_dplx_rec` field and installs
  a process-global allocator hook. Its runtime behavior is exercised by the
  RPCSEC_GSS integration job against Ubuntu 24.04's libntirpc 6.3 (both client
  and server). The scheduled `libntirpc-abi` job additionally negotiates
  RPCSEC_GSS with a client linked against a pinned newer upstream libntirpc,
  exercising the reply-verifier shim, `rdma_call_expires` layout and allocator
  hook at runtime (the NFS server in that job remains the distribution
  package). Treat the feature as version-bounded and re-run the RPCSEC_GSS
  integration test whenever libntirpc is upgraded.
- The reply-verifier shim replaces the client transport's `xp_ops` table.
  libntirpc reads that pointer with plain, unlocked loads, and
  `clnt_vc_ncreatef` registers the transport with its worker before the shim
  can be installed, so the swap is **not race-free**. `vfsi-nfs` installs as
  early as possible and the installer refuses to run while a client request is
  already outstanding, but a small window remains. A race-free install would
  require registering the transport only after the shim is in place, which
  libntirpc's client path does not expose. Treat `rpcsec-gss` as best-effort on
  this path. The long-term fix is upstream
  (https://github.com/nfs-ganesha/ntirpc/pull/414): populate `clnt_req.cc_verf`
  from the decoded reply verifier inside libntirpc itself, removing the need
  for this shim.
- The validated platform is Linux/glibc; other platforms are untested.
