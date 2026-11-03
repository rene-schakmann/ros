# Build environment for ROS: Rust toolchain, disk image tools and QEMU.
# Used by the top-level Makefile, so the host only needs Docker.
FROM rust:1.99.1-slim-trixie

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        make mtools dosfstools fdisk file \
        qemu-system-arm qemu-efi-aarch64 python3 \
 && rm -rf /var/lib/apt/lists/*

# Install the components and targets listed in rust-toolchain.toml at image
# build time, so containers don't download them on every run.
COPY rust-toolchain.toml /tmp/toolchain/
RUN cd /tmp/toolchain && rustup toolchain install && rustup show active-toolchain

WORKDIR /ros
