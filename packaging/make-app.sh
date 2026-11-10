#!/usr/bin/env bash
# Builds Kumu.app — a real macOS application bundle.
#
# `cargo run` starts a bare Mach-O executable, so the Dock icon is only the one
# src/dock_icon.rs sets on the running NSApplication at launch, and macOS
# releases it the moment the process tears down: quit from the Dock and the tile
# briefly falls back to the executable's generic icon. A bundle carries
# `CFBundleIconFile` in its Info.plist, which LaunchServices owns independently
# of the process, so the icon survives quitting (see packaging/Info.plist).
#
# The binary embeds its assets (rust-embed), so the bundle is just the
# executable, the Info.plist and the .icns. Agents are still discovered at
# runtime; if one is not on the GUI-launched PATH, set OMP_BIN / OPENCODE_BIN /
# CODEX_BIN / CLAUDE_BIN / PI_BIN (src/agent/config.rs).
#
# Signing: set CODESIGN_IDENTITY to a "Developer ID Application: …" identity
# for a locally signed bundle (hardened runtime). Unset / "-" keeps an ad-hoc
# seal for local use. Distributable DMGs go through packaging/release.sh,
# which cloud-signs the exported app instead.
#
# Usage: packaging/make-app.sh [release|debug] [--open]
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=lib.sh
source packaging/lib.sh

profile=release
open_after=false
for arg in "$@"; do
    case "$arg" in
        release|debug) profile="$arg" ;;
        --open) open_after=true ;;
        *) echo "usage: $0 [release|debug] [--open]" >&2; exit 2 ;;
    esac
done

if [[ "$profile" == release ]]; then
    cargo build --release
    target_dir=target/release
else
    cargo build
    target_dir=target/debug
fi

bundle="$target_dir/Kumu.app"

echo "Assembling $bundle"
rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"

cp "$target_dir/kumu" "$bundle/Contents/MacOS/kumu"
cp assets/AppIcon.icns "$bundle/Contents/Resources/AppIcon.icns"
cp packaging/Info.plist "$bundle/Contents/Info.plist"

# Keep the bundle's version in step with Cargo.toml.
version="$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -n1)"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$bundle/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" "$bundle/Contents/Info.plist"
plutil -lint "$bundle/Contents/Info.plist" >/dev/null

sign_app_bundle "$bundle"

echo "Built $bundle"
if [[ "$open_after" == true ]]; then
    open "$bundle"
fi
