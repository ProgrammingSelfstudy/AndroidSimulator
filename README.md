# moniqi

Lightweight Android emulator for macOS, built on Apple's Virtualization.framework (no QEMU).
Runs on **Intel and Apple silicon** Macs: the guest always matches the host CPU.

## Build

Requires Rust (rustup). From the repo root:

```sh
scripts/build-universal.sh          # dist/moniqi: x86_64 + arm64, ad-hoc signed
# or just this machine:
cargo build --release && scripts/sign.sh
```

The guest PID 1 and agent are static Linux binaries, cross-compiled with `rust-lld`:

```sh
scripts/build-guest.sh              # this Mac's arch; pass arm64 or x86_64 to choose
```

## Quick start

```sh
moniqi create dev --cpus 4 --memory-mib 2048
moniqi build-image dev \
  --guest-init  target/<arch>-unknown-linux-musl/release/moniqi-init \
  --guest-agent target/<arch>-unknown-linux-musl/release/moniqi-agent
moniqi pull-android dev
moniqi start dev                    # opens the window; Ctrl-C shuts down cleanly
```

`<arch>` is `aarch64` on Apple silicon and `x86_64` on Intel.

## Installing apps

Drag one or more `.apk` files onto the emulator window to install them, or:

```sh
moniqi install app.apk
```

Both use `adb install -r -t`, so `adb` must be on PATH
(`brew install --cask android-platform-tools`).

## Tuning

```sh
moniqi config dev --resolution 540x960 --dpi 240 --fps 30 --cpus 4 --memory-mib 2048
```

Rendering is software, so fewer pixels and a lower refresh rate cost less CPU.
The default is 30 fps; 60 fps with software encoding saturates the CPU and is not worth it.
Config changes that touch the guest agent need `build-image` re-run.

adb is forwarded to `127.0.0.1:5555`.
