# SUM RPC Server and Protocoll
This crate contains the server and protocol of the example SUM RPC program.

## The server binary

The server is a standalone binary that can be made with `make sum_server`.  To
test sum, we need to build and run the server first. The sever binary should be
found in the OUT_DIR of this crate after this crate is built using the provided
`build.rs` script.

If the OUT_DIR is difficult to find we can just run `make sum_server` in the src
directory to build a server binary over there.

To launch the server, simply run `./sum_server &`.

## The protocol library

The protocol library will be built as a C library by `make libsumlib.a`. Then,
Rust binding to the library will be generated and built into a Rust library.
