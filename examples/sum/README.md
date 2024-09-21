# About

This is the client-side of the example sum RPC program.
Its server and protocol are inside the `sum_lib` crate.

It depends on not only the `sum_lib` library but also two C libraries:

1. `tirpc`: this is a system library that can be installed with `apt install
libtirpc-dev`.

2. `sumlib`: this is a C library that is built by the `sum_lib` crate.

These two dependencies are included in the `build.rs` script of this crate.