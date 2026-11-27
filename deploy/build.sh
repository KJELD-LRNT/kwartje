#!/bin/sh
# Release build for Debian 12 (glibc 2.36), inside a debian:12-based Rust
# container so the binary runs on the LXC. The 512 MB LXC is too small to
# compile on. Output: target/release/kwartje
set -eu
cd "$(dirname "$0")/.."
docker run --rm \
  -v "$PWD":/src -w /src \
  -v kwartje-cargo-registry:/usr/local/cargo/registry \
  -e SQLX_OFFLINE=true \
  rust:1-bookworm \
  sh -c 'cargo build --release --locked && strip target/release/kwartje'
ls -lh target/release/kwartje
