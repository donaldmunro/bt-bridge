# Build container for compiling bt-bridge binaries that can run on Ubuntu-derived
# and other distributions that were built using an earlier glibc than the 
# one used for rolling distributions such as Arch/Fedora (glibc is not
# downwardly compatatible). To check glibc version on a system, run:
#   ldd --version or getconf GNU_LIBC_VERSION. 
# Building inside this ubuntu:22.04 container (glibc 2.35) gives binaries 
# that run on Ubuntu 22.04 and anything newer, while binaries build 
# in Arch/Fedora are currently glibc 2.43.
#
# Usage (from the repo root):
#   docker build -t bt-bridge-builder -f ancillary/scripts/Dockerfile .
#   docker run --rm -v "$PWD":/work \
#       -v bt-bridge-cargo-registry:/root/.cargo/registry \
#       bt-bridge-builder cargo build --release --locked
#
# Binaries are output in the (local) target-docker/release/ directory on the host
# (owned by root so a chown may be necessary).

FROM ubuntu:22.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential curl ca-certificates pkg-config libdbus-1-dev \
    && rm -rf /var/lib/apt/lists/*

# Match the nightly toolchain used on the dev machines (no rust-toolchain.toml
# in the repo, so the default toolchain is what gets used).
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --default-toolchain nightly --profile minimal

ENV PATH=/root/.cargo/bin:$PATH
ENV CARGO_TARGET_DIR=/work/target-docker

WORKDIR /work
