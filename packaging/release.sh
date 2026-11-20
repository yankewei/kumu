#!/usr/bin/env bash
# Builds, signs, notarizes and verifies a distributable Kumu DMG.
# `--publish` additionally tags the commit and uploads the GitHub Release.
#
# Usage: packaging/release.sh [--publish] [--keep]
#   --publish  tag v<version>, push it, and create the GitHub Release
#   --keep     keep the intermediate directory even on success
#
# Signing, best first:
#   KUMU_SIGNING_IDENTITY="Developer ID Application: …"   (already in a keychain)
#   KUMU_SIGNING_P12=path.p12 KUMU_SIGNING_P12_PASSWORD=… (temp keychain)
#   a "Developer ID Application: …" identity in the keychain
# Without one the app is exported from an Xcode archive with cloud signing,
# which needs Xcode and is about a minute slower.
#
# Only the DMG is notarized: the app inside is Developer ID signed and covered
# by the DMG's ticket, and the app is not stapled separately. Credentials:
#   xcrun notarytool store-credentials kumu --apple-id <Apple ID> --team-id <Team ID>
# Artifact names feed the updater's matching: do not rename.
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=lib.sh
source packaging/lib.sh

publish=false
keep=false
for arg in "$@"; do
    case "$arg" in
        --publish) publish=true ;;
        --keep) keep=true ;;
        -h|--help) sed -n '2,19p' "$0"; exit 0 ;;
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
version="$(sed -n '/^version =/{s/^version = "\([^"]*\)".*/\1/p; q;}' Cargo.toml)"
[[ -n "$version" ]] || die "无法从 Cargo.toml 读取版本号"
tag="v$version"

arch="$(uname -m)"
case "$arch" in
    arm64|aarch64) arch=aarch64 ;;
    x86_64) arch=x86_64 ;;
    *) die "不支持的架构：$arch" ;;
esac

step "1/6 预检（${tag}, ${arch}）"
xcrun --find codesign >/dev/null
xcrun --find notarytool >/dev/null
xcrun --find stapler >/dev/null
if ! history="$(xcrun notarytool history --keychain-profile "$profile" 2>&1)"; then
    echo "$history" >&2
    echo "公证凭据不可用。一次性配置：" >&2
    echo "  xcrun notarytool store-credentials $profile --apple-id '<Apple ID>' --team-id $team" >&2
    exit 1
fi
if [[ "$publish" == true ]]; then
    gh auth status >/dev/null || die "GitHub CLI 未登录；先运行 gh auth login"
    [[ -z "$(git status --porcelain)" ]] \
        || die "工作区不干净；--publish 只接受已提交的代码"
    ! git rev-parse -q --verify "refs/tags/$tag" >/dev/null \
        || die "标签 $tag 已存在"
    remote_tag="$(git ls-remote --tags origin "refs/tags/$tag")" \
        || die "无法查询远端标签 $tag"
    [[ -z "$remote_tag" ]] || die "远端已有标签 $tag"
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/kumu-release.XXXXXX")"
searched=()
while IFS= read -r entry; do
    [[ -n "$entry" ]] && searched+=("$entry")
done < <(security list-keychains -d user | sed -e 's/^[[:space:]]*"//' -e 's/"[[:space:]]*$//')
keychain=""
cleanup() {
    local status=$?
    if [[ -n "$keychain" ]]; then
        security list-keychains -d user -s ${searched[@]+"${searched[@]}"} >/dev/null 2>&1 || true
        security delete-keychain "$keychain" >/dev/null 2>&1 || true
    fi
    if ((status != 0)); then
        echo "release: 失败，保留工作目录：$work" >&2
    elif [[ "$keep" == true ]]; then
        echo "release: 保留工作目录：$work"
    else
        rm -rf "$work"
    fi
}
trap cleanup EXIT

# A Developer ID identity signs the app and the DMG locally, which keeps a
# release at about two minutes; without one the app goes through Xcode's cloud
# signing, which is slower and needs Xcode signed in to the team.
signing_identity=""
if [[ -n "${KUMU_SIGNING_IDENTITY:-}" ]]; then
    signing_identity="$KUMU_SIGNING_IDENTITY"
elif [[ -n "${KUMU_SIGNING_P12:-}" ]]; then
    [[ -f "$KUMU_SIGNING_P12" ]] || die "KUMU_SIGNING_P12 不是文件：$KUMU_SIGNING_P12"
    : "${KUMU_SIGNING_P12_PASSWORD:?KUMU_SIGNING_P12 需要 KUMU_SIGNING_P12_PASSWORD}"
    keychain="$work/signing.keychain-db"
    keychain_password="$(openssl rand -hex 32)"
    security create-keychain -p "$keychain_password" "$keychain"
    security set-keychain-settings -lut 21600 "$keychain"
    security unlock-keychain -p "$keychain_password" "$keychain"
    security import "$KUMU_SIGNING_P12" -k "$keychain" \
        -P "$KUMU_SIGNING_P12_PASSWORD" -T /usr/bin/codesign -T /usr/bin/security
    security set-key-partition-list -S apple-tool:,apple: -s \
        -k "$keychain_password" "$keychain" >/dev/null
    security list-keychains -d user -s "$keychain" ${searched[@]+"${searched[@]}"}
    # The p12 is explicit, so read the identity it carries even when its chain
    # is not one the system trusts yet (certificates, not validity, name it).
    signing_identity="$(security find-identity -p codesigning "$keychain" 2>/dev/null \
        | sed -n '/"Developer ID Application: /{s/.*"\(Developer ID Application: [^"]*\)".*/\1/p; q;}')"
    [[ -n "$signing_identity" ]] || die "p12 里没有 Developer ID Application 身份"
elif [[ "${CODESIGN_IDENTITY:-}" == "Developer ID Application: "* ]]; then
    # A general local-build setting is only used when it is a release identity.
    signing_identity="$CODESIGN_IDENTITY"
else
    signing_identity="$(security find-identity -v -p codesigning 2>/dev/null \
        | sed -n '/"Developer ID Application: /{s/.*"\(Developer ID Application: [^"]*\)".*/\1/p; q;}')"
fi
if [[ -n "$signing_identity" ]]; then
    [[ "$signing_identity" == "Developer ID Application: "* ]] \
        || die "KUMU_SIGNING_IDENTITY 不是 Developer ID Application：$signing_identity"
    export CODESIGN_IDENTITY="$signing_identity"
    echo "签名身份：${signing_identity}（本地）"
else
    # Clear any non-Developer-ID CODESIGN_IDENTITY from the caller's environment
    # so `sign_dmg` later does not sign the DMG with an unrelated certificate.
    unset CODESIGN_IDENTITY
    echo "没有 Developer ID 身份，改用 Xcode 云端签名；设置 KUMU_SIGNING_P12 可走本地签名"
fi

check_signed_app() {
    local app="$1" signature authority
    codesign --verify --deep --strict "$app"
    signature="$(codesign -dv --verbose=4 "$app" 2>&1)"
    authority="$(sed -n '/^Authority=/{s/^Authority=//p; q;}' <<<"$signature")"
    [[ "$authority" == "Developer ID Application: "* ]] \
        || die "应用没有使用 Developer ID 签名（Authority=${authority:-无}）"
    grep -q 'flags=.*runtime' <<<"$signature" \
        || die "应用没有 hardened runtime"
    echo "签名：$authority"
}

step "2/6 编译、组包并签名"
if [[ -n "$signing_identity" ]]; then
    packaging/make-app.sh release
    app="target/release/Kumu.app"
    check_signed_app "$app"
else
    development_identity="${KUMU_DEVELOPMENT_IDENTITY:-}"
    if [[ -z "$development_identity" ]]; then
        development_identity="$(security find-identity -v -p codesigning 2>/dev/null \
            | sed -n '/"Apple Development: /{s/.*"\(Apple Development: [^"]*\)".*/\1/p; q;}')"
    fi
    [[ -n "$development_identity" ]] \
        || die "钥匙串里没有 Apple Development 证书；先在 Xcode 登录团队 $team"
    cargo build --release
    KUMU_RELEASE_DIR="$work" KUMU_VERSION="$version" KUMU_TEAM_ID="$team" \
        KUMU_DEVELOPMENT_IDENTITY="$development_identity" python3 packaging/make-archive.py
    app="$work/Kumu.xcarchive/Products/Applications/Kumu.app"
    codesign --force --options runtime --timestamp \
        --entitlements packaging/entitlements.plist \
        --sign "$development_identity" \
        "$app"
fi

if [[ -z "$signing_identity" ]]; then
    step "3/6 Xcode 云端 Developer ID 签名"
    echo "如弹出钥匙串或账号授权，请在桌面上允许。"
    xcodebuild -exportArchive \
        -archivePath "$work/Kumu.xcarchive" \
        -exportOptionsPlist "$work/ExportOptions.plist" \
        -exportPath "$work/export" \
        -allowProvisioningUpdates
    app="$work/export/Kumu.app"
    check_signed_app "$app"
else
    step "3/6 本地 Developer ID 已签名，跳过云端导出"
fi

notarize() {
    local file="$1" kind="$2" output="$3" id attempt
    if ! xcrun notarytool submit "$file" \
        --keychain-profile "$profile" --wait \
        | tee "$work/$kind-submission.txt"; then
        echo "release: 公证上传失败或超时。网络问题常见，可对同一文件重新 submit，不必重新编译：" >&2
        echo "  xcrun notarytool submit \"$file\" --keychain-profile $profile --wait" >&2
        exit 1
    fi
    id="$(sed -n '/^  id: /{s/^  id: //p; q;}' "$work/$kind-submission.txt")"
    [[ -n "$id" ]] || die "没读到 $kind 的 submission id（见 $work/$kind-submission.txt）"
    xcrun notarytool log "$id" \
        --keychain-profile "$profile" \
        "$work/$kind-notarization.json"
    python3 - "$work/$kind-notarization.json" <<'PY'
import json
import sys

with open(sys.argv[1]) as fh:
    result = json.load(fh)
if result.get("status") != "Accepted" or result.get("issues"):
    raise SystemExit(f"公证未通过：{sys.argv[1]}")
PY
    # The notary service reports Accepted before the ticket reaches the CDN,
    # so an immediate staple can still miss it. Retry stapling the same
    # artifact instead of re-notarizing it.
    for attempt in 1 2 3 4 5; do
        if xcrun stapler staple "$output" && xcrun stapler validate "$output"; then
            return 0
        fi
        if ((attempt < 5)); then
            echo "release: 票据尚未同步，20 秒后重试 stapler staple（${kind}）" >&2
            sleep 20
        fi
    done
    die "stapler staple/validate 失败：$output"
}

step "4/6 制作、签名并公证 DMG"
mkdir "$work/dmg-stage"
ditto "$app" "$work/dmg-stage/Kumu.app"
ln -s /Applications "$work/dmg-stage/Applications"
dmg="$work/Kumu-${version}-${arch}.dmg"
hdiutil create -volname Kumu \
    -srcfolder "$work/dmg-stage" -ov -format UDZO "$dmg"
sign_dmg "$dmg"
notarize "$dmg" dmg "$dmg"
hdiutil verify "$dmg"
if [[ -n "$signing_identity" ]]; then
    gatekeeper="$(spctl --assess --type open --context context:primary-signature \
        --verbose=2 "$dmg" 2>&1)"
    grep -q 'source=Notarized Developer ID' <<<"$gatekeeper" \
        || die "Gatekeeper 未认可 DMG：$gatekeeper"
fi

step "5/6 挂载复验并保存产物"
KUMU_RELEASE_DIR="$work" KUMU_VERSION="$version" \
    KUMU_APP="$app" KUMU_DMG="$dmg" \
    python3 packaging/verify-release.py
artifact="target/release/$(basename "$dmg")"

step "6/6 GitHub Release"
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
