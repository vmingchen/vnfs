FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive

LABEL org.opencontainers.image.description="Build environment for a pinned upstream libntirpc used to exercise the libntirpc-sys ABI probes"

# Only the build dependencies for libntirpc itself; the Rust workspace builds on
# the host so the runner's Cargo cache and toolchain are reused.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        cmake \
        g++ \
        gcc \
        libgssglue-dev \
        libkrb5-dev \
        liburcu-dev \
        make \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*
