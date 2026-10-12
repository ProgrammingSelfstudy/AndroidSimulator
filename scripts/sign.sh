#!/usr/bin/env bash
# Ad-hoc sign the binary with the virtualization entitlement (required by VZ).
set -euo pipefail
BIN="${1:-target/release/moniqi}"
codesign --force --sign - --entitlements "$(dirname "$0")/../entitlements.plist" "$BIN"
codesign -d --entitlements - "$BIN" 2>&1 | grep -q virtualization && echo "signed: $BIN"
