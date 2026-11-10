#!/usr/bin/env bash
# Builds, cloud-signs, notarizes and verifies a distributable Kumu DMG.
# `--publish` additionally tags the commit and uploads the GitHub Release.
#
# Usage: packaging/release.sh [--publish] [--keep]
#   --publish  tag v<version>, push it, and create the GitHub Release
#   --keep     keep the intermediate directory even on success
#
# Requires a full Xcode signed into an account with Developer ID access for
# team 3BUD4GCZCQ, and notarization credentials in keychain profile kumu:
#   xcrun notarytool store-credentials kumu --apple-id <Apple ID> --team-id 3BUD4GCZCQ
# No local Developer ID private key is needed: the archive is signed with the
# Apple Development certificate and Xcode swaps in the cloud Developer ID.
# Signing and export can ask for keychain access; approve it on the desktop.
# The app and the DMG are notarized separately, then the DMG is verified again
# from the inside. Artifact names feed the updater's matching: do not rename.
set -euo pipefail
cd "$(dirname "$0")/.."

publish=false
keep=false
for arg in "$@"; do
    case "$arg" in
        --publish) publish=true ;;
        --keep) keep=true ;;
        -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
        *)
            echo "usage: $0 [--publish] [--keep]" >&2
            exit 2
            ;;
    esac
done

step() { printf '\n== %s\n' "$*"; }
die() { echo "release: $*" >&2; exit 1; }

team="${KUMU_TEAM_ID:-3BUD4GCZCQ}"
profile="${KUMU_NOTARY_PROFILE:-kumu}"
version="$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -n1)"
[[ -n "$version" ]] || die "无法从 Cargo.toml 读取版本号"
tag="v$version"

arch="$(uname -m)"
case "$arch" in
    arm64|aarch64) arch=aarch64 ;;
    x86_64) arch=x86_64 ;;
    *) die "不支持的架构：$arch" ;;
esac

# The archive is signed with the local Apple Development certificate; Xcode
# swaps in the cloud-managed Developer ID during -exportArchive.
identity="${KUMU_DEVELOPMENT_IDENTITY:-}"
if [[ -z "$identity" ]]; then
    identity="$(security find-identity -v -p codesigning 2>/dev/null \
        | sed -n 's/.*"\(Apple Development: .*\)"/\1/p' \
        | head -n1)"
fi
[[ -n "$identity" ]] || die "钥匙串里没有 Apple Development 证书；先在 Xcode 登录团队 $team"

step "1/7 预检（${tag}, ${arch}）"
xcodebuild -version
xcrun --find notarytool >/dev/null
xcrun --find stapler >/dev/null
if ! history="$(xcrun notarytool history --keychain-profile "$profile" 2>&1)"; then
    echo "$history" >&2
    echo "公证凭据不可用。一次性配置：" >&2
    echo "  xcrun notarytool store-credentials $profile --apple-id '<Apple ID>' --team-id $team" >&2
    exit 1
fi
if [[ "$publish" == true ]]; then
    [[ -z "$(git status --porcelain)" ]] \
        || die "工作区不干净；--publish 只接受已提交的代码"
    ! git rev-parse -q --verify "refs/tags/$tag" >/dev/null \
        || die "标签 $tag 已存在"
    remote_tag="$(git ls-remote --tags origin "refs/tags/$tag")" \
        || die "无法查询远端标签 $tag"
    [[ -z "$remote_tag" ]] || die "远端已有标签 $tag"
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/kumu-release.XXXXXX")"
cleanup() {
    local status=$?
    if ((status != 0)); then
        echo "release: 失败，保留工作目录：$work" >&2
    elif [[ "$keep" == true ]]; then
        echo "release: 保留工作目录：$work"
    else
        rm -rf "$work"
    fi
}
trap cleanup EXIT

step "2/7 编译、归档并用开发证书签名"
cargo build --release
KUMU_RELEASE_DIR="$work" KUMU_VERSION="$version" KUMU_TEAM_ID="$team" \
    KUMU_DEVELOPMENT_IDENTITY="$identity" python3 packaging/make-archive.py
app="$work/Kumu.xcarchive/Products/Applications/Kumu.app"
codesign --force --options runtime --timestamp \
    --entitlements packaging/entitlements.plist \
    --sign "$identity" \
    "$app"

step "3/7 Xcode 云端 Developer ID 签名"
echo "如弹出钥匙串或账号授权，请在桌面上允许。"
xcodebuild -exportArchive \
    -archivePath "$work/Kumu.xcarchive" \
    -exportOptionsPlist "$work/ExportOptions.plist" \
    -exportPath "$work/export" \
    -allowProvisioningUpdates
app="$work/export/Kumu.app"
codesign --verify --deep --strict "$app"
signature="$(codesign -dv --verbose=4 "$app" 2>&1)"
authority="$(sed -n 's/^Authority=//p' <<<"$signature" | head -n1)"
[[ "$authority" == "Developer ID Application: "* ]] \
    || die "导出后未使用 Developer ID 签名（Authority=${authority:-无}）"
grep -q 'flags=.*runtime' <<<"$signature" \
    || die "导出后的应用没有 hardened runtime"
echo "签名：$authority"

notarize() {
    local file="$1" kind="$2" output="$3" id
    if ! xcrun notarytool submit "$file" \
        --keychain-profile "$profile" --wait \
        | tee "$work/$kind-submission.txt"; then
        echo "release: 公证上传失败或超时。网络问题常见，可对同一文件重新 submit，不必重新编译：" >&2
        echo "  xcrun notarytool submit \"$file\" --keychain-profile $profile --wait" >&2
        exit 1
    fi
    id="$(sed -n 's/^  id: //p' "$work/$kind-submission.txt" | head -n1)"
    [[ -n "$id" ]] || die "没读到 $kind 的 submission id（见 $work/$kind-submission.txt）"
    xcrun notarytool log "$id" \
        --keychain-profile "$profile" \
        "$work/$kind-notarization.json"
    python3 - "$work/$kind-notarization.json" <<'PY'
import json
import sys

result = json.load(open(sys.argv[1]))
if result.get("status") != "Accepted" or result.get("issues"):
    raise SystemExit(f"公证未通过：{sys.argv[1]}")
PY
    xcrun stapler staple "$output"
    xcrun stapler validate "$output"
}

step "4/7 公证应用并附加票据"
ditto -c -k --keepParent "$app" "$work/Kumu.zip"
notarize "$work/Kumu.zip" app "$app"
gatekeeper="$(spctl --assess --type execute --verbose=2 "$app" 2>&1)"
grep -q 'source=Notarized Developer ID' <<<"$gatekeeper" \
    || die "Gatekeeper 未认可应用：$gatekeeper"

step "5/7 制作并公证 DMG"
mkdir "$work/dmg-stage"
ditto "$app" "$work/dmg-stage/Kumu.app"
ln -s /Applications "$work/dmg-stage/Applications"
dmg="$work/Kumu-${version}-${arch}.dmg"
hdiutil create -volname Kumu \
    -srcfolder "$work/dmg-stage" -ov -format UDZO "$dmg"
notarize "$dmg" dmg "$dmg"
hdiutil verify "$dmg"

step "6/7 挂载复验并保存产物"
KUMU_RELEASE_DIR="$work" KUMU_VERSION="$version" \
    KUMU_APP="$app" KUMU_DMG="$dmg" \
    python3 packaging/verify-release.py
artifact="target/release/$(basename "$dmg")"

step "7/7 GitHub Release"
if [[ "$publish" != true ]]; then
    echo "跳过打标签和上传；确认安装包后用 --publish 发布 $tag"
    exit 0
fi
gh auth status
git tag -a "$tag" -m "Release $tag"
git push origin HEAD "refs/tags/$tag"
gh release create "$tag" \
    "$artifact" \
    "$artifact.sha256" \
    target/release/SHA256SUMS \
    --title "$tag" --generate-notes --verify-tag
echo "已发布 $tag"
