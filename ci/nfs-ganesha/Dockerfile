FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive
ARG GANESHA_REPOSITORY=https://github.com/vfsi/nfs-ganesha.git
ARG GANESHA_REVISION=ec1993746f815f32396f0de36cc7614658a33173

LABEL org.opencontainers.image.source="https://github.com/vfsi/nfs-ganesha"
LABEL org.opencontainers.image.revision="${GANESHA_REVISION}"
LABEL org.opencontainers.image.description="Pinned NFS-Ganesha build for vnfs server-side COPY integration tests"

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bison \
        build-essential \
        ca-certificates \
        cmake \
        flex \
        git \
        libacl1-dev \
        libblkid-dev \
        libcap-dev \
        libdbus-1-dev \
        libfl-dev \
        libkrb5-dev \
        libnfsidmap-dev \
        libssl-dev \
        libsystemd-dev \
        libtirpc-common \
        liburcu-dev \
        pkg-config \
        python3 \
        uuid-dev \
    && rm -rf /var/lib/apt/lists/*

RUN git init /usr/src/nfs-ganesha \
    && git -C /usr/src/nfs-ganesha remote add origin "${GANESHA_REPOSITORY}" \
    && git -C /usr/src/nfs-ganesha fetch --depth=1 origin "${GANESHA_REVISION}" \
    && git -C /usr/src/nfs-ganesha checkout --detach FETCH_HEAD \
    && test "$(git -C /usr/src/nfs-ganesha rev-parse HEAD)" = "${GANESHA_REVISION}" \
    && git -C /usr/src/nfs-ganesha submodule update --init --recursive --depth=1 src/libntirpc

RUN cmake \
        -S /usr/src/nfs-ganesha/src \
        -B /tmp/ganesha-build \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/usr \
        -DUSE_9P=OFF \
        -DUSE_DBUS=ON \
        -DUSE_FSAL_VFS=ON \
        -DUSE_GSS=OFF \
        -DUSE_MONITORING=ON \
        -DUSE_NFS3=OFF \
        -DUSE_RQUOTA=OFF \
    && cmake --build /tmp/ganesha-build --parallel \
    && cmake --install /tmp/ganesha-build \
    && rm -rf /tmp/ganesha-build /usr/src/nfs-ganesha

COPY ganesha.conf /etc/ganesha/ganesha.conf

RUN mkdir -p /export /run/ganesha /var/lib/nfs/ganesha /var/log/ganesha

EXPOSE 2049/tcp

ENTRYPOINT ["/usr/bin/ganesha.nfsd", "-F", "-x", "-L", "STDERR", "-f", "/etc/ganesha/ganesha.conf"]
