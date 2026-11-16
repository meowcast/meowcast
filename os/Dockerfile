FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential gcc-arm-linux-gnueabihf binutils-arm-linux-gnueabihf \
    libc6-dev-armhf-cross flex bison bc libssl-dev libelf-dev \
    python3 cpio xz-utils bzip2 ca-certificates device-tree-compiler \
    qemu-system-arm && rm -rf /var/lib/apt/lists/*
WORKDIR /work
