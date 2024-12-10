#!/bin/sh

# WARNING: cross may download target-specific images from the internet
# WARNING: cargo msrv may install toolchains via rustup
set -ex

cargo fmt --check

cargo test
cargo test --no-default-features
cargo test --no-default-features --features lz4,dmabuf
cargo test --no-default-features --features zstd,video

# Tier 1
cross test --target x86_64-unknown-linux-gnu --no-default-features
cross test --target aarch64-unknown-linux-gnu --no-default-features

# Linux support, 32 bit
cross test --target i686-unknown-linux-gnu --no-default-features
cross test --target armv7-unknown-linux-gnueabihf --no-default-features

# Big-endian representative
cross test --target powerpc64-unknown-linux-gnu --no-default-features

# FreeBSD support (testing not available, needs full emulation?)
cross build --target x86_64-unknown-freebsd --no-default-features
cross build --target i686-unknown-freebsd --no-default-features

# Check that the build still works with older Rust versions
cargo msrv verify
