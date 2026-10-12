#!/usr/bin/env bash
# Build a self-contained bundle for Apple silicon Macs: dist/moniqi-m3.tar.gz
# (arm64 moniqi + aarch64 guest init/agent + setup/start scripts). Works from an Intel Mac too:
# everything is cross-compiled; the kernel and Android image are downloaded on the target.
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=dist/moniqi-m3
rustup target list --installed | grep -qx aarch64-apple-darwin || rustup target add aarch64-apple-darwin
cargo build --release --target aarch64-apple-darwin -p moniqi
scripts/build-guest.sh arm64

rm -rf "$OUT" dist/moniqi-m3.tar.gz
mkdir -p "$OUT/bin" "$OUT/guest"
cp target/aarch64-apple-darwin/release/moniqi "$OUT/bin/moniqi"
cp target/aarch64-unknown-linux-musl/release/moniqi-init target/aarch64-unknown-linux-musl/release/moniqi-agent "$OUT/guest/"
scripts/sign.sh "$OUT/bin/moniqi"

cat > "$OUT/setup.sh" <<'SETUP'
#!/bin/bash
# One-time setup of a moniqi VM on this Mac (Apple silicon). Needs internet for the downloads.
set -euo pipefail
cd "$(dirname "$0")"
xattr -dr com.apple.quarantine . 2>/dev/null || true
M=./bin/moniqi
NAME="${1:-dev}"
if [ -f "$HOME/.moniqi/$NAME/vm.json" ]; then
  echo "VM '$NAME' already exists in ~/.moniqi/$NAME: reusing it (data disk is kept)."
else
  $M create "$NAME" --cpus 4 --memory-mib 2048 --disk-gib 16
fi
$M build-image "$NAME" --guest-init guest/moniqi-init --guest-agent guest/moniqi-agent
$M pull-android "$NAME"
$M config "$NAME" --resolution 540x960 --dpi 240 --fps 20 --trim on
echo
echo "done. start it with:  ./bin/moniqi start $NAME"
SETUP
cat > "$OUT/start.sh" <<'START'
#!/bin/bash
cd "$(dirname "$0")"
exec ./bin/moniqi start "${1:-dev}"
START
cat > "$OUT/README.txt" <<'README'
moniqi for Apple silicon (M1/M2/M3...) - macOS 13 or later

1. ./setup.sh            first time only; downloads the kernel and Android image (~1 GB)
2. ./start.sh            opens the emulator window (Ctrl-C in the terminal = clean shutdown)

Install apps: drag an .apk onto the window, or  ./bin/moniqi install app.apk
  (needs adb:  brew install --cask android-platform-tools)
Tune:  ./bin/moniqi config dev --resolution 720x1280 --dpi 320 --fps 30   (restart to apply)
Networking is built in (follows the Mac's own VPN/proxy); adb is on 127.0.0.1:5555.

If macOS says the binary is damaged/unverified:
  xattr -dr com.apple.quarantine .
README
chmod +x "$OUT/setup.sh" "$OUT/start.sh" "$OUT/bin/moniqi"
(cd dist && tar czf moniqi-m3.tar.gz moniqi-m3)
ls -lh dist/moniqi-m3.tar.gz
