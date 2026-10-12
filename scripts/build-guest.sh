#!/usr/bin/env bash
# Cross-compile the guest PID 1 and agent (static, musl) on macOS using rust-lld.
# Usage: scripts/build-guest.sh [arm64|x86_64]   (default: this machine's arch)
set -euo pipefail
cd "$(dirname "$0")/.."
case "${1:-$(uname -m)}" in
  arm64|aarch64) TARGET=aarch64-unknown-linux-musl ;;
  x86_64|amd64)  TARGET=x86_64-unknown-linux-musl ;;
  *) echo "unknown arch: $1" >&2; exit 1 ;;
esac
rustup target list --installed | grep -qx "$TARGET" || rustup target add "$TARGET"
LLD_DIR="$(dirname "$(find "$(rustc --print sysroot)" -name rust-lld -type f | head -1)")"
[ -x "$LLD_DIR/rust-lld" ] || { echo "rust-lld not found in the Rust toolchain" >&2; exit 1; }
PATH="$LLD_DIR:$PATH" cargo build --release -p moniqi-guest-init -p moniqi-guest-agent --target "$TARGET"
for bin in moniqi-init moniqi-agent; do
  OUT="target/$TARGET/release/$bin"
  file "$OUT" | grep -q "statically linked" || { echo "$bin: not a static binary" >&2; exit 1; }
  echo "built: $OUT"
done
