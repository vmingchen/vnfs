# nfsv41-sys

Raw FFI bindings for the NFSv4.1 protocol, vendored from NFS-Ganesha's
`nfsv41.h`. This crate provides the protocol data types (`COMPOUND4args`,
`nfs_argop4`, `nfs_resop4`, ...) and the XDR codecs that encode/decode them,
as a low-level foundation for the `vnfs` client.

This is a `-sys` crate: it exposes unsafe `extern` types and functions with
manual memory management. Higher-level, safe wrappers live in the `vnfs`
crate.

## What's in the box

| File | Purpose |
|------|---------|
| `src/nfsv41.h` | NFS-Ganesha's NFSv4.1 protocol header (structs + static-inline XDR codecs) |
| `src/wrapper.c` | Five `xdr_wrap_*` extern entry points around the `static inline` codecs (Rust cannot call static-inline functions directly) |
| `src/wrapper.h` | The bindgen input header (includes `nfsv41.h`, declares the wrappers) |
| `src/lib.rs` | `include!` of the generated bindings, plus the `-lntirpc` link directive |
| `build.rs` | Compiles `wrapper.c` into `libnfsv41.a` and runs bindgen |

## Build

`build.rs` performs two steps:

1. **C codec archive**: `gcc` compiles `src/wrapper.c` (with libntirpc's
   include directories, supplied by `libntirpc-sys` via its `links`
   metadata as `DEP_NTIRPC_INCLUDE` / `DEP_NTIRPC_INCLUDE2`) into
   `libnfsv41.a`.
2. **bindgen**: generates ~15k lines of Rust declarations from
   `src/wrapper.h` into `$OUT_DIR/bindings.rs`, which `src/lib.rs` includes.

The archive's object files reference libntirpc symbols, so the crate
re-emits the vendored static `libntirpc.a` at final link time. Consumers do
not need libntirpc on the dynamic linker path at runtime.

## Usage

```rust
use nfsv41_sys::*;
use std::os::raw::c_char;

unsafe {
    let mut argop: nfs_argop4 = std::mem::zeroed();
    argop.argop = nfs_opnum4_NFS4_OP_PUTFH;
    argop.nfs_argop4_u.opputfh.object.nfs_fh4_len = 4;
    argop.nfs_argop4_u.opputfh.object.nfs_fh4_val = fh.as_ptr() as *mut c_char;

    let mut args: COMPOUND4args = std::mem::zeroed();
    // ... wire up tag / argarray ...

    let mut xdr: XDR = std::mem::zeroed();
    xdrmem_ncreate(&mut xdr, buf.as_mut_ptr() as *mut c_char, BUF, xdr_op_XDR_ENCODE);
    xdr_wrap_COMPOUND4args(&mut xdr, &mut args);
}
```

## Tests

`tests/roundtrip.rs` encodes a `COMPOUND4args`/`COMPOUND4res` (one PUTFH op)
with the codecs, decodes it back, and checks the fields match. Run with:

```sh
cargo test -p nfsv41-sys
```

## Improvement roadmap

- **Pure-Rust codecs via `xdrgen`.** The workspace already carries
  `protocol/nfsv41.x` (the RFC 5661 XDR spec) and the `xdrgen` /
  `xdr-codec` dependencies. `xdrgen` parses the `.x` spec and emits typed
  `Pack`/`Unpack` implementations, which would remove gcc, `wrapper.c`,
  bindgen, and the `-Wno-incompatible-pointer-types` workaround. See below.
- **Safe wrapper layer.** Owned buffer types (`Vec<u8>`, `String`), RAII for
  decoded trees (no manual `xdr_free_null_stream`), and safe accessors for
  the `nfs_argop4_u` / `nfs_resop4_u` unions.
- **Narrow the bindings.** An allowlist of NFS types (instead of blocklists)
  would shrink the generated surface and compile time.
- **More codec tests.** Round-trips cover PUTFH plus the basic ops
  (READ, WRITE, LOOKUP, READDIR, REMOVE, RENAME, CREATE, GETFH, READLINK,
  GETATTR, COMMIT, LINK) in `tests/ops_roundtrip.rs`; property-based
  round-trips across the remaining ops would catch more XDR bugs.

### How xdrgen generates Rust

`xdrgen` (used together with the `xdr-codec` runtime) turns an RFC 4506 /
RFC 5661 XDR specification into plain Rust source:

1. **Parse.** The XDR grammar is parsed (xdrgen's parser is built on
   `nom`) into an AST of typedefs, structs, unions, enums, constants, and
   program/version blocks.
2. **Emit.** The AST is lowered into Rust source text (xdrgen uses `quote`)
   written to `$OUT_DIR/<name>_xdr.rs`. For every type it emits a matching
   `struct` (or `enum` for unions) plus:
   - `impl Pack for T` — the encoder, following the XDR wire rules
     (4-byte alignment, variable-length `<>` prefixes, discriminants for
     unions),
   - `impl Unpack for T` — the decoder, allocating owned `Vec`s/`String`s
     as it goes.
3. **Wire up.** The generated file is included with
   `include!(concat!(env!("OUT_DIR"), "/nfsv41_xdr.rs"))` inside a module,
   with `xdr-codec` as a dependency. Types the generator can't express
   (e.g. the `%`-passthrough `authsys_parms`) get hand-written
   `Pack`/`Unpack` impls.

In a `build.rs` the whole thing is one call:

```rust
fn main() {
    xdrgen::compile("protocol/nfsv41.x").unwrap();
}
```

For NFSv4.1 the practical caveat is that `xdrgen` 0.4.4 is an old crate
(deps from the `quote` 0.3 / `nom` 3 era); the complex NFSv4.1 unions and
the `authsys_parms` passthrough may need manual impls, and the generator
itself may need a build fix before the raw bindings can be retired.
