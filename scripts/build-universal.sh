#!/usr/bin/env bash
# Build moniqi for both Intel and Apple silicon Macs, merge into one universal binary and sign it.
# Usage: scripts/build-universal.sh        -> dist/moniqi
set -euo pipefail
cd "$(dirname "$0")/.."
for t in aarch64-apple-darwin x86_64-apple-darwin; do
  rustup target list --installed | grep -qx "$t" || rustup target add "$t"
  cargo build --release -p moniqi --target "$t"
done
mkdir -p dist
lipo -create -output dist/moniqi \
  target/aarch64-apple-darwin/release/moniqi target/x86_64-apple-darwin/release/moniqi
scripts/sign.sh dist/moniqi
lipo -info dist/moniqi
