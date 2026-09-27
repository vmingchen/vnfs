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

Normal builds discover the administrator-provided `libntirpc` 6.3 or newer
through `pkg-config` and generate Rust declarations from its installed
headers. The build script does not access the network or download native
source. The currently supported native target is Linux.

| Dependency | Package (Ubuntu) | Purpose |
| --- | --- | --- |
| libntirpc | `libntirpc-dev` | RPC implementation and headers (6.3+) |
| pkg-config | `pkg-config` | locate the installed library and headers |
| clang | `clang` | provide the headers used by bindgen to generate bindings |
| GSSAPI headers | `libkrb5-dev` | only for the optional `rpcsec-gss` feature |

Install them on Ubuntu with:

```sh
sudo apt install clang libclang-dev pkg-config libntirpc-dev
```

Add `libkrb5-dev` only when building the `rpcsec-gss` feature:

```sh
sudo apt install libkrb5-dev
```

The `libntirpc` shared object is linked against `liburcu-bp` and, when the
distribution enables it, `libgssapi_krb5`. The runtime `libntirpc6.3` package
depends on both, so a default build needs no extra development packages.

docs.rs uses checked-in declarations and therefore does not require native
packages. Those declarations are only a documentation input; normal builds
always bind to the locally installed headers.
