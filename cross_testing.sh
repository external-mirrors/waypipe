#!/bin/sh

# WARNING: cross may download target-specific images from the internet
# WARNING: cargo msrv may install toolchains via rustup
set -ex

cargo fmt --check

cargo test
cargo test --no-default-features
cargo test --no-default-features --features lz4,dmabuf,test_proto
cargo test --no-default-features --features zstd,video,test_proto
cargo test --no-default-features --features gbmfallback,test_proto

# Tier 1
cross test --target x86_64-unknown-linux-gnu --no-default-features --features dmabuf,test_proto
cross test --target aarch64-unknown-linux-gnu --no-default-features --features dmabuf,test_proto

# Linux support, 32 bit
cross test --target i686-unknown-linux-gnu --no-default-features --features dmabuf,test_proto
cross test --target armv7-unknown-linux-gnueabihf --no-default-features --features dmabuf,test_proto

# Big-endian representative
cross test --target powerpc64-unknown-linux-gnu --no-default-features --features test_proto

# FreeBSD support (testing not available, needs full emulation?)
cross build --target x86_64-unknown-freebsd --no-default-features --features test_proto
cross build --target i686-unknown-freebsd --no-default-features --features test_proto

# Check that the build still works with older Rust versions
cargo msrv verify

# on 64 bit systems with the necessary libraries, cross build to 32 bit
# PKG_CONFIG_ALLOW_CROSS=1 cargo build --target=i686-unknown-linux-gnu
