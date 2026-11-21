# Shared helpers for packaging/*.sh (sourced, not executed).
# shellcheck shell=bash

# Prefer an explicit CODESIGN_IDENTITY ("-" = ad-hoc).
# make-app.sh/make-dmg.sh default to ad-hoc for local packages; release.sh
# exports a Developer ID identity when it has one and notarizes the DMG.
codesign_identity() {
    if [[ -n "${CODESIGN_IDENTITY:-}" ]]; then
        echo "$CODESIGN_IDENTITY"
        return
    fi
    echo "-"
}

# Sign Kumu.app. Developer ID builds get hardened runtime + timestamp +
# entitlements (required for notarization); ad-hoc stays a simple seal.
sign_app_bundle() {
    local bundle="$1"
    local identity entitlements
    identity="$(codesign_identity)"
    entitlements="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/entitlements.plist"

    if [[ "$identity" == "-" ]]; then
        echo "Signing $bundle (ad-hoc)"
        codesign --force --sign - "$bundle"
        return
    fi

    echo "Signing $bundle as $identity"
    codesign --force --options runtime --timestamp \
        --entitlements "$entitlements" \
        --sign "$identity" \
        "$bundle/Contents/MacOS/kumu"
    codesign --force --options runtime --timestamp \
        --entitlements "$entitlements" \
        --sign "$identity" \
        "$bundle"
    codesign --verify --deep --strict "$bundle"
}

# Sign a DMG when using a real identity (Gatekeeper checks the disk image too).
sign_dmg() {
    local dmg="$1"
    local identity
    identity="$(codesign_identity)"
    if [[ "$identity" == "-" ]]; then
        return
    fi
    echo "Signing $dmg as $identity"
    codesign --force --timestamp --sign "$identity" "$dmg"
    codesign --verify --strict "$dmg"
}

# Resolve xcrun notarytool auth flags into global NOTARY_AUTH_ARGS / NOTARY_AUTH_DESC.
# Prefers App Store Connect API key when configured; otherwise uses a keychain
# profile (explicit env var, or optional $1 default such as "kumu").
notary_auth_args() {
    local default_profile="${1:-}"
    local profile="${KUMU_NOTARY_PROFILE:-${APPLE_NOTARY_PROFILE:-$default_profile}}"
    NOTARY_AUTH_ARGS=()
    NOTARY_AUTH_DESC=""

    if [[ -n "${APPLE_API_KEY_PATH:-}" && -n "${APPLE_API_KEY_ID:-}" && -n "${APPLE_API_ISSUER_ID:-}" ]]; then
        NOTARY_AUTH_ARGS=(
            --key "$APPLE_API_KEY_PATH"
            --key-id "$APPLE_API_KEY_ID"
            --issuer "$APPLE_API_ISSUER_ID"
        )
        NOTARY_AUTH_DESC="App Store Connect API key (${APPLE_API_KEY_ID})"
        return 0
    fi

    if [[ -n "$profile" ]]; then
        NOTARY_AUTH_ARGS=(--keychain-profile "$profile")
        NOTARY_AUTH_DESC="keychain profile (${profile})"
        return 0
    fi

    return 1
}

# The notary service can report Accepted before the ticket reaches the CDN,
# so an immediate staple can miss it. Retry stapling the same artifact instead
# of re-notarizing it.
staple_with_retry() {
    local output="$1"
    local label="${2:-artifact}"
    local attempt
    for attempt in 1 2 3 4 5; do
        if xcrun stapler staple "$output" && xcrun stapler validate "$output"; then
            return 0
        fi
        if ((attempt < 5)); then
            echo "release: 票据尚未同步，20 秒后重试 stapler staple（${label}）" >&2
            sleep 20
        fi
    done
    echo "release: stapler staple/validate 失败：${output}" >&2
    return 1
}

# Submit a file to Apple notarization, optionally verify its JSON log, then
# staple and validate the output artifact.
# Usage: notarize_file <file> <output> [submission_log] [json_log] [label] [default_profile]
notarize_file() {
    local file="$1"
    local output="$2"
    local submission_log="${3:-}"
    local json_log="${4:-}"
    local label="${5:-artifact}"
    local default_profile="${6:-}"
    local id

    if ! notary_auth_args "$default_profile"; then
        echo "Notarization credentials not configured." >&2
        return 1
    fi

    echo "Notarizing ${file} (${NOTARY_AUTH_DESC})"
    if [[ -n "$submission_log" ]]; then
        if ! xcrun notarytool submit "$file" "${NOTARY_AUTH_ARGS[@]}" --wait \
            | tee "$submission_log"; then
            echo "release: 公证上传失败或超时。网络问题常见，可对同一文件重新 submit，不必重新编译：" >&2
            printf '  xcrun notarytool submit %q' "$file" >&2
            printf ' %q' "${NOTARY_AUTH_ARGS[@]}" >&2
            printf ' --wait\n' >&2
            return 1
        fi
    else
        if ! xcrun notarytool submit "$file" "${NOTARY_AUTH_ARGS[@]}" --wait; then
            echo "release: 公证上传失败或超时。可对同一文件重新 submit：" >&2
            printf '  xcrun notarytool submit %q' "$file" >&2
            printf ' %q' "${NOTARY_AUTH_ARGS[@]}" >&2
            printf ' --wait\n' >&2
            return 1
        fi
    fi

    if [[ -n "$json_log" ]]; then
        [[ -n "$submission_log" ]] || {
            echo "release: json_log 需要同时提供 submission_log" >&2
            return 1
        }
        id="$(sed -n '/^  id: /{s/^  id: //p; q;}' "$submission_log")"
        if [[ -z "$id" ]]; then
            echo "release: 没读到 ${label} 的 submission id（见 ${submission_log}）" >&2
            return 1
        fi
        xcrun notarytool log "$id" "${NOTARY_AUTH_ARGS[@]}" "$json_log"
        python3 - "$json_log" <<'PY'
import json
import sys

with open(sys.argv[1]) as fh:
    result = json.load(fh)
if result.get("status") != "Accepted" or result.get("issues"):
    raise SystemExit(f"公证未通过：{sys.argv[1]}")
PY
    fi

    staple_with_retry "$output" "$label"
}

# Notarize + staple for local make-dmg.sh runs.
# Skips unless APPLE_NOTARY_PROFILE / KUMU_NOTARY_PROFILE or APPLE_API_KEY_* is set.
notarize_and_staple() {
    local path="$1"
    if [[ "$(codesign_identity)" == "-" ]]; then
        if [[ "${REQUIRE_NOTARIZATION:-}" == 1 ]]; then
            echo "Cannot notarize an ad-hoc signature" >&2
            return 1
        fi
        return
    fi

    if ! notary_auth_args ""; then
        if [[ "${REQUIRE_NOTARIZATION:-}" == 1 ]]; then
            echo "Notarization required but no credentials configured." >&2
            echo "One-time setup:" >&2
            echo "  xcrun notarytool store-credentials \"kumu\" --apple-id \"…\" --team-id \"…\" --password \"app-specific-password\"" >&2
            return 1
        fi
        echo "Skipping notarization (set APPLE_NOTARY_PROFILE=kumu for local releases)"
        return
    fi

    notarize_file "$path" "$path" "" "" "dmg" ""
}
