#!/usr/bin/env bash
# Local signed + notarized GitHub Release — no GitHub Actions secrets.
#
# Uses the Developer ID in your login keychain and a notarytool keychain
# profile (default name: kumu). One-time setup is in the README.
#
# Usage:
#   packaging/release.sh              # version from Cargo.toml
#   packaging/release.sh --dry-run    # build/sign/notarize only, no tag/upload
set -euo pipefail
cd "$(dirname "$0")/.."

dry_run=false
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry_run=true ;;
        -h|--help)
            echo "usage: $0 [--dry-run]" >&2
            exit 0
            ;;
        *) echo "usage: $0 [--dry-run]" >&2; exit 2 ;;
    esac
done

if ! command -v gh >/dev/null; then
    echo "gh (GitHub CLI) is required: brew install gh && gh auth login" >&2
    exit 1
fi

version="$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -n1)"
if [[ -z "$version" ]]; then
    echo "Could not read version from Cargo.toml" >&2
    exit 1
fi
tag="v${version}"

if [[ -n "$(git status --porcelain)" ]]; then
    echo "Working tree is dirty; commit or stash before releasing." >&2
    git status --short >&2
    exit 1
fi

export REQUIRE_DEVELOPER_ID=1
export REQUIRE_NOTARIZATION=1
export APPLE_NOTARY_PROFILE="${APPLE_NOTARY_PROFILE:-kumu}"

echo "Building signed + notarized DMG for $tag"
packaging/make-dmg.sh release

arch="$(uname -m)"
case "$arch" in
    arm64|aarch64) arch=aarch64 ;;
    x86_64) arch=x86_64 ;;
esac
dmg="target/release/Kumu-${version}-${arch}.dmg"
if [[ ! -f "$dmg" ]]; then
    echo "Expected DMG missing: $dmg" >&2
    exit 1
fi

(
    cd "$(dirname "$dmg")"
    shasum -a 256 "$(basename "$dmg")" > "$(basename "$dmg").sha256"
    cp "$(basename "$dmg").sha256" SHA256SUMS
)

if [[ "$dry_run" == true ]]; then
    echo "Dry run: left $dmg (not tagged / not uploaded)"
    exit 0
fi

if git rev-parse "$tag" >/dev/null 2>&1; then
    echo "Tag $tag already exists locally" >&2
    exit 1
fi

git tag -a "$tag" -m "Release $tag"
git push origin HEAD "refs/tags/$tag"

gh release create "$tag" "$dmg" "${dmg}.sha256" target/release/SHA256SUMS \
    --title "$tag" \
    --generate-notes

echo "Published $tag with $(basename "$dmg")"
