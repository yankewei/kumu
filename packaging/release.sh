#!/usr/bin/env bash
# Builds, signs, notarizes and verifies a distributable Kumu DMG.
# `--publish` additionally tags the commit, pushes, and uploads a GitHub Release
# (Draft by default so Release notes can be finalized before the updater sees it).
#
# Usage: packaging/release.sh [options]
#   --publish            tag v<version>, push it, and create a GitHub Release (Draft by default)
#   --public             with --publish: publish immediately instead of creating a Draft Release
#   --notes-file <path>  with --publish: use custom Release notes instead of --generate-notes
#   --rebuild            with --publish: force a fresh build/notarization instead of reusing a verified DMG
#   --skip-ci-check      with --publish: skip verifying that GitHub CI succeeded on HEAD
#   --keep               keep the intermediate directory even on success
#
# Signing, best first:
#   KUMU_SIGNING_IDENTITY="Developer ID Application: …"             (already in a keychain)
#   KUMU_SIGNING_P12=path.p12 + KUMU_SIGNING_P12_PASSWORD_FILE=…    (temp keychain; password not exported)
#   ~/.kumu-signing/developer-id.p12 + developer-id.p12.password    (auto-discovered local cert)
#   a "Developer ID Application: …" identity in the keychain
# Without one the app is exported from an Xcode archive with cloud signing,
# which needs Xcode and is about a minute slower.
#
# Only the DMG is notarized: the app inside is Developer ID signed and covered
# by the DMG's ticket, and the app is not stapled separately. Credentials:
#   xcrun notarytool store-credentials kumu --apple-id <Apple ID> --team-id <Team ID>
#   or APPLE_API_KEY_PATH + APPLE_API_KEY_ID + APPLE_API_ISSUER_ID
# Artifact names feed the updater's matching: do not rename.
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=lib.sh
source packaging/lib.sh

publish=false
draft=true
notes_file=""
rebuild=false
skip_ci_check=false
keep=false

while (($# > 0)); do
    case "$1" in
        --publish) publish=true ;;
        --public|--no-draft) draft=false ;;
        --draft) draft=true ;;
        --notes-file)
            shift
            (($# > 0)) || { echo "release: --notes-file 需要传入文件路径" >&2; exit 2; }
            notes_file="$1"
            ;;
        --notes-file=*)
            notes_file="${1#--notes-file=}"
            ;;
        --rebuild) rebuild=true ;;
        --skip-ci-check) skip_ci_check=true ;;
        --keep) keep=true ;;
        -h|--help) sed -n '2,26p' "$0"; exit 0 ;;
        *)
            echo "usage: $0 [--publish] [--public] [--notes-file <path>] [--rebuild] [--skip-ci-check] [--keep]" >&2
            exit 2
            ;;
    esac
    shift
done

step() { printf '\n== %s\n' "$*"; }
die() { echo "release: $*" >&2; exit 1; }

if [[ "$publish" != true ]]; then
    [[ "$draft" == true ]] || die "--public / --no-draft 只能配合 --publish 使用"
    [[ -z "$notes_file" ]] || die "--notes-file 只能配合 --publish 使用"
    [[ "$rebuild" != true ]] || die "--rebuild 只能配合 --publish 使用"
    [[ "$skip_ci_check" != true ]] || die "--skip-ci-check 只能配合 --publish 使用"
fi
if [[ -n "$notes_file" ]]; then
    [[ -f "$notes_file" ]] || die "--notes-file 不是有效文件：${notes_file}"
    [[ -s "$notes_file" ]] || die "--notes-file 为空：${notes_file}"
fi

team="${KUMU_TEAM_ID:-3BUD4GCZCQ}"
profile="${KUMU_NOTARY_PROFILE:-kumu}"
release_branch="${KUMU_RELEASE_BRANCH:-master}"
version="$(sed -n '/^version =/{s/^version = "\([^"]*\)".*/\1/p; q;}' Cargo.toml)"
[[ -n "$version" ]] || die "无法从 Cargo.toml 读取版本号"
tag="v${version}"

arch="$(uname -m)"
case "$arch" in
    arm64|aarch64) arch=aarch64 ;;
    x86_64) arch=x86_64 ;;
    *) die "不支持的架构：${arch}" ;;
esac

head_commit="$(git rev-parse HEAD)"
if [[ -z "$(git status --porcelain)" ]]; then
    is_dirty=false
else
    is_dirty=true
fi

step "1/6 预检（${tag}, ${arch}）"

# Guard against releasing with a dirty or non-vt-* Ghostty submodule pointer.
[[ -d vendor/ghostty/.git || -f vendor/ghostty/.git ]] \
    || die "缺少 vendor/ghostty 子模块；请先 git submodule update --init --recursive"
[[ -z "$(git -C vendor/ghostty status --porcelain)" ]] \
    || die "vendor/ghostty 工作区不干净，请先清理后再打包"
ghostty_tag="$(git -C vendor/ghostty describe --tags --exact-match HEAD 2>/dev/null || true)"
[[ "$ghostty_tag" == vt-* ]] \
    || die "vendor/ghostty 必须钉在 vt-* 标签上（当前：${ghostty_tag:-未在标签上}）"
echo "Ghostty 子模块：${ghostty_tag}"

ahead_count=0
if [[ "$publish" == true ]]; then
    gh auth status >/dev/null || die "GitHub CLI 未登录；先运行 gh auth login"
    current_branch="$(git symbolic-ref --quiet --short HEAD || true)"
    [[ "$current_branch" == "$release_branch" ]] \
        || die "当前分支为 ${current_branch:-detached HEAD}，--publish 只能在 ${release_branch} 分支执行（可用 KUMU_RELEASE_BRANCH 覆盖）"
    [[ "$is_dirty" == false ]] \
        || die "工作区不干净；--publish 只接受已提交的代码"
    git fetch --quiet origin "$release_branch" \
        || die "无法拉取远端分支 origin/${release_branch}"
    behind_count="$(git rev-list --count "HEAD..origin/${release_branch}")"
    [[ "$behind_count" == "0" ]] \
        || die "本地 ${release_branch} 落后 origin/${release_branch} ${behind_count} 个提交，请先同步后再发布"
    ahead_count="$(git rev-list --count "origin/${release_branch}..HEAD")"

    ! git rev-parse -q --verify "refs/tags/${tag}" >/dev/null \
        || die "本地标签 ${tag} 已存在"
    remote_tag="$(git ls-remote --tags origin "refs/tags/${tag}")" \
        || die "无法查询远端标签 ${tag}"
    [[ -z "$remote_tag" ]] || die "远端已有标签 ${tag}"
    ! gh release view "$tag" >/dev/null 2>&1 \
        || die "GitHub Release ${tag} 已存在"

    if [[ "$skip_ci_check" != true ]]; then
        if [[ "$ahead_count" != "0" ]]; then
            die "本地有 ${ahead_count} 个未推送提交，CI 尚未校验当前 HEAD；请先 git push 等 CI 通过，或使用 --skip-ci-check 跳过"
        fi
        ci_runs_json="$(gh run list --workflow ci.yml --commit "$head_commit" --limit 20 --json status,conclusion,url,createdAt)" \
            || die "无法查询提交 ${head_commit:0:12} 的 GitHub CI 状态（可传 --skip-ci-check 跳过）"
        python3 - "$ci_runs_json" "$head_commit" <<'PY'
import json
import sys

runs = json.loads(sys.argv[1])
commit = sys.argv[2][:12]
if not runs:
    raise SystemExit(
        f"release: 未找到提交 {commit} 的 ci.yml 运行记录；请先确认已 push 并等待 CI 完成（或传 --skip-ci-check 跳过）"
    )
latest = sorted(runs, key=lambda r: r.get("createdAt", ""), reverse=True)[0]
status = latest.get("status")
conclusion = latest.get("conclusion")
url = latest.get("url", "")
if status != "completed" or conclusion != "success":
    raise SystemExit(
        f"release: 提交 {commit} 的 CI 尚未通过（status={status}, conclusion={conclusion}）{url}\n"
        "请等待 CI 绿灯后再发布，或传 --skip-ci-check 显式跳过。"
    )
print(f"CI 已通过（{commit}）：{url}")
PY
    else
        echo "已跳过 CI 状态检查（--skip-ci-check）"
    fi
fi

artifact="target/release/Kumu-${version}-${arch}.dmg"
reuse_existing=false
if [[ "$publish" == true && "$rebuild" != true ]]; then
    if reuse_msg="$(KUMU_VERSION="$version" KUMU_ARCH="$arch" KUMU_COMMIT="$head_commit" \
        python3 packaging/verify-release.py --check-existing 2>&1)"; then
        reuse_existing=true
        echo "$reuse_msg"
    else
        echo "无法复用现有产物，将重新构建并公证（原因：${reuse_msg}）"
    fi
fi

if [[ "$reuse_existing" != true ]]; then
    xcrun --find codesign >/dev/null
    xcrun --find notarytool >/dev/null
    xcrun --find stapler >/dev/null
    notary_auth_args "$profile" || die "未配置公证凭据"
    if ! history="$(xcrun notarytool history "${NOTARY_AUTH_ARGS[@]}" 2>&1)"; then
        echo "$history" >&2
        echo "公证凭据不可用（${NOTARY_AUTH_DESC}）。一次性配置：" >&2
        echo "  xcrun notarytool store-credentials ${profile} --apple-id '<Apple ID>' --team-id ${team}" >&2
        echo "  或设置 APPLE_API_KEY_PATH + APPLE_API_KEY_ID + APPLE_API_ISSUER_ID" >&2
        exit 1
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
            echo "release: 失败，保留工作目录：${work}" >&2
        elif [[ "$keep" == true ]]; then
            echo "release: 保留工作目录：${work}"
        else
            rm -rf "$work"
        fi
    }
    trap cleanup EXIT

    # A Developer ID identity signs the app and the DMG locally, which keeps a
    # release at about two minutes; without one the app goes through Xcode's
    # cloud signing, which is slower and needs Xcode signed in to the team.
    signing_identity=""
    p12_path=""
    p12_password=""
    default_p12="${HOME}/.kumu-signing/developer-id.p12"
    default_p12_pw="${HOME}/.kumu-signing/developer-id.p12.password"

    if [[ -n "${KUMU_SIGNING_IDENTITY:-}" ]]; then
        signing_identity="$KUMU_SIGNING_IDENTITY"
    elif [[ -n "${KUMU_SIGNING_P12:-}" ]]; then
        p12_path="$KUMU_SIGNING_P12"
        [[ -f "$p12_path" ]] || die "KUMU_SIGNING_P12 不是文件：${p12_path}"
        if [[ -n "${KUMU_SIGNING_P12_PASSWORD_FILE:-}" ]]; then
            [[ -f "$KUMU_SIGNING_P12_PASSWORD_FILE" ]] \
                || die "KUMU_SIGNING_P12_PASSWORD_FILE 不是文件：${KUMU_SIGNING_P12_PASSWORD_FILE}"
            p12_password="$(tr -d '\r\n' < "$KUMU_SIGNING_P12_PASSWORD_FILE")"
        elif [[ -n "${KUMU_SIGNING_P12_PASSWORD:-}" ]]; then
            p12_password="$KUMU_SIGNING_P12_PASSWORD"
        elif [[ -f "${p12_path}.password" ]]; then
            p12_password="$(tr -d '\r\n' < "${p12_path}.password")"
        else
            die "KUMU_SIGNING_P12 需要 KUMU_SIGNING_P12_PASSWORD_FILE（或 KUMU_SIGNING_P12_PASSWORD / ${p12_path}.password）"
        fi
    elif [[ -f "$default_p12" && -f "$default_p12_pw" ]]; then
        p12_path="$default_p12"
        p12_password="$(tr -d '\r\n' < "$default_p12_pw")"
    elif [[ "${CODESIGN_IDENTITY:-}" == "Developer ID Application: "* ]]; then
        # A general local-build setting is only used when it is a release identity.
        signing_identity="$CODESIGN_IDENTITY"
    else
        signing_identity="$(security find-identity -v -p codesigning 2>/dev/null \
            | sed -n '/"Developer ID Application: /{s/.*"\(Developer ID Application: [^"]*\)".*/\1/p; q;}')"
    fi

    if [[ -n "$p12_path" ]]; then
        [[ -n "$p12_password" ]] || die "p12 密码为空：${p12_path}"
        keychain="$work/signing.keychain-db"
        keychain_password="$(openssl rand -hex 32)"
        security create-keychain -p "$keychain_password" "$keychain"
        security set-keychain-settings -lut 21600 "$keychain"
        security unlock-keychain -p "$keychain_password" "$keychain"
        security import "$p12_path" -k "$keychain" \
            -P "$p12_password" -T /usr/bin/codesign -T /usr/bin/security
        unset p12_password
        security set-key-partition-list -S apple-tool:,apple: -s \
            -k "$keychain_password" "$keychain" >/dev/null
        security list-keychains -d user -s "$keychain" ${searched[@]+"${searched[@]}"}
        # The p12 is explicit, so read the identity it carries even when its chain
        # is not one the system trusts yet (certificates, not validity, name it).
        signing_identity="$(security find-identity -p codesigning "$keychain" 2>/dev/null \
            | sed -n '/"Developer ID Application: /{s/.*"\(Developer ID Application: [^"]*\)".*/\1/p; q;}')"
        [[ -n "$signing_identity" ]] || die "p12 里没有 Developer ID Application 身份"
    fi

    if [[ -n "$signing_identity" ]]; then
        [[ "$signing_identity" == "Developer ID Application: "* ]] \
            || die "KUMU_SIGNING_IDENTITY 不是 Developer ID Application：${signing_identity}"
        export CODESIGN_IDENTITY="$signing_identity"
        echo "签名身份：${signing_identity}（本地）"
    else
        # Clear any non-Developer-ID CODESIGN_IDENTITY from the caller's environment
        # so `sign_dmg` later does not sign the DMG with an unrelated certificate.
        unset CODESIGN_IDENTITY
        echo "没有 Developer ID 身份，改用 Xcode 云端签名；配置 ~/.kumu-signing/ 或 KUMU_SIGNING_P12 可走本地签名"
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
        echo "签名：${authority}"
    }

    step "2/6 编译、组包并签名"
    if [[ -n "$signing_identity" ]]; then
        packaging/make-app.sh release --locked
        app="target/release/Kumu.app"
        check_signed_app "$app"
    else
        development_identity="${KUMU_DEVELOPMENT_IDENTITY:-}"
        if [[ -z "$development_identity" ]]; then
            development_identity="$(security find-identity -v -p codesigning 2>/dev/null \
                | sed -n '/"Apple Development: /{s/.*"\(Apple Development: [^"]*\)".*/\1/p; q;}')"
        fi
        [[ -n "$development_identity" ]] \
            || die "钥匙串里没有 Apple Development 证书；先在 Xcode 登录团队 ${team}"
        cargo build --release --locked
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

    step "4/6 制作、签名并公证 DMG"
    mkdir "$work/dmg-stage"
    ditto "$app" "$work/dmg-stage/Kumu.app"
    ln -s /Applications "$work/dmg-stage/Applications"
    dmg="$work/Kumu-${version}-${arch}.dmg"
    hdiutil create -volname Kumu \
        -srcfolder "$work/dmg-stage" -ov -format UDZO "$dmg"
    sign_dmg "$dmg"
    notarize_file "$dmg" "$dmg" \
        "$work/dmg-submission.txt" "$work/dmg-notarization.json" \
        dmg "$profile"
    hdiutil verify "$dmg"
    if [[ -n "$signing_identity" ]]; then
        gatekeeper="$(spctl --assess --type open --context context:primary-signature \
            --verbose=2 "$dmg" 2>&1)"
        grep -q 'source=Notarized Developer ID' <<<"$gatekeeper" \
            || die "Gatekeeper 未认可 DMG：${gatekeeper}"
    fi

    step "5/6 挂载复验并保存产物"
    KUMU_RELEASE_DIR="$work" KUMU_VERSION="$version" \
        KUMU_ARCH="$arch" KUMU_COMMIT="$head_commit" KUMU_DIRTY="$is_dirty" \
        KUMU_APP="$app" KUMU_DMG="$dmg" \
        python3 packaging/verify-release.py
    artifact="target/release/$(basename "$dmg")"
else
    step "2/6-5/6 复用已验证产物，跳过编译、签名、公证与挂载复验"
fi

step "6/6 GitHub Release"
if [[ "$publish" != true ]]; then
    echo "跳过打标签和上传；确认安装包后运行以下任一命令发布 ${tag}（将直接复用本次产物）："
    echo "  packaging/release.sh --publish"
    echo "  packaging/release.sh --publish --notes-file <notes.md>"
    exit 0
fi

gh auth status >/dev/null
if [[ "$ahead_count" != "0" ]]; then
    git push origin "HEAD:${release_branch}"
fi

git tag -a "$tag" -m "Release ${tag}"
if ! git push origin "refs/tags/${tag}"; then
    git tag -d "$tag" >/dev/null 2>&1 || true
    die "推送标签 ${tag} 失败，已清理本地临时标签"
fi

release_cmd=(
    gh release create "$tag"
    "$artifact"
    "${artifact}.sha256"
    target/release/SHA256SUMS
    --title "$tag"
    --verify-tag
)
if [[ "$draft" == true ]]; then
    release_cmd+=(--draft)
fi
if [[ -n "$notes_file" ]]; then
    release_cmd+=(--notes-file "$notes_file")
else
    release_cmd+=(--generate-notes)
fi

if ! "${release_cmd[@]}"; then
    echo "release: 标签 ${tag} 已推送，但创建 GitHub Release 失败。修复网络或权限后可直接重试：" >&2
    printf '  %q' "${release_cmd[@]}" >&2
    printf '\n' >&2
    exit 1
fi

if [[ "$draft" == true ]]; then
    echo "已创建 Draft Release ${tag}（应用内更新器暂不可见）。"
    echo "补完面向用户的 Release notes 后，执行以下命令正式对外发布："
    echo "  gh release edit ${tag} --notes-file - --draft=false <<'MD'"
    echo "  ...发布说明..."
    echo "  MD"
else
    echo "已正式发布 ${tag}"
    if [[ -z "$notes_file" ]]; then
        echo "提醒：当前使用 --generate-notes 生成说明，请及时用 gh release edit ${tag} 替换为面向用户的发布说明。"
    fi
fi
