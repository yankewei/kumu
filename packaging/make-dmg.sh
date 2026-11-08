#!/usr/bin/env bash
# Builds a drag-to-Applications DMG from Kumu.app.
#
# Layout inside the volume:
#   Kumu.app
#   Applications -> /Applications
#
# When CODESIGN_IDENTITY is a Developer ID Application identity, the app and
# DMG are signed with hardened runtime. When APPLE_API_KEY_PATH /
# APPLE_API_KEY_ID / APPLE_API_ISSUER_ID are set, the DMG is notarized and
# stapled (required for Gatekeeper-clean downloads).
#
# Usage: packaging/make-dmg.sh [release|debug]
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=lib.sh
source packaging/lib.sh

profile=release
for arg in "$@"; do
    case "$arg" in
        release|debug) profile="$arg" ;;
        *) echo "usage: $0 [release|debug]" >&2; exit 2 ;;
    esac
done

packaging/make-app.sh "$profile"

if [[ "$profile" == release ]]; then
    target_dir=target/release
else
    target_dir=target/debug
fi

version="$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -n1)"
arch="$(uname -m)"
case "$arch" in
    arm64|aarch64) arch=aarch64 ;;
    x86_64) arch=x86_64 ;;
esac

app="$target_dir/Kumu.app"
dmg="$target_dir/Kumu-${version}-${arch}.dmg"
stage="$(mktemp -d "${TMPDIR:-/tmp}/kumu-dmg.XXXXXX")"
cleanup() { rm -rf "$stage"; }
trap cleanup EXIT

mkdir -p "$stage"
cp -R "$app" "$stage/Kumu.app"
ln -s /Applications "$stage/Applications"

rm -f "$dmg"
hdiutil create \
    -volname "Kumu" \
    -srcfolder "$stage" \
    -ov \
    -format UDZO \
    "$dmg"

sign_dmg "$dmg"
notarize_and_staple "$dmg"

echo "Built $dmg"
