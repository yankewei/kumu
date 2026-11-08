# Shared helpers for packaging/*.sh (sourced, not executed).
# shellcheck shell=bash

# Prefer an explicit CODESIGN_IDENTITY ("-" = ad-hoc).
# When unset and REQUIRE_DEVELOPER_ID=1, pick the first Developer ID Application
# identity from the keychain (local release path — no GitHub secrets).
codesign_identity() {
    if [[ -n "${CODESIGN_IDENTITY:-}" ]]; then
        echo "$CODESIGN_IDENTITY"
        return
    fi
    if [[ "${REQUIRE_DEVELOPER_ID:-}" == 1 ]]; then
        local id
        id="$(security find-identity -v -p codesigning 2>/dev/null \
            | sed -n 's/.*"\(Developer ID Application: .*\)"/\1/p' \
            | head -n1)"
        if [[ -z "$id" ]]; then
            echo "No Developer ID Application identity in keychain." >&2
            echo "Install one via Xcode → Settings → Accounts → Manage Certificates," >&2
            echo "or set CODESIGN_IDENTITY explicitly." >&2
            return 1
        fi
        echo "$id"
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

# Notarize + staple.
# Preferred (no secrets files): APPLE_NOTARY_PROFILE=kumu  (notarytool keychain profile)
# Fallback: APPLE_API_KEY_PATH + APPLE_API_KEY_ID + APPLE_API_ISSUER_ID
notarize_and_staple() {
    local path="$1"
    if [[ "$(codesign_identity)" == "-" ]]; then
        if [[ "${REQUIRE_NOTARIZATION:-}" == 1 ]]; then
            echo "Cannot notarize an ad-hoc signature" >&2
            return 1
        fi
        return
    fi

    if [[ -n "${APPLE_NOTARY_PROFILE:-}" ]]; then
        echo "Notarizing $path (keychain profile: $APPLE_NOTARY_PROFILE)"
        xcrun notarytool submit "$path" \
            --keychain-profile "$APPLE_NOTARY_PROFILE" \
            --wait
    elif [[ -n "${APPLE_API_KEY_PATH:-}" && -n "${APPLE_API_KEY_ID:-}" && -n "${APPLE_API_ISSUER_ID:-}" ]]; then
        echo "Notarizing $path (App Store Connect API key)"
        xcrun notarytool submit "$path" \
            --key "$APPLE_API_KEY_PATH" \
            --key-id "$APPLE_API_KEY_ID" \
            --issuer "$APPLE_API_ISSUER_ID" \
            --wait
    else
        if [[ "${REQUIRE_NOTARIZATION:-}" == 1 ]]; then
            echo "Notarization required but no credentials configured." >&2
            echo "One-time setup:" >&2
            echo "  xcrun notarytool store-credentials \"kumu\" --apple-id \"…\" --team-id \"…\" --password \"app-specific-password\"" >&2
            return 1
        fi
        echo "Skipping notarization (set APPLE_NOTARY_PROFILE=kumu for local releases)"
        return
    fi

    xcrun stapler staple "$path"
    xcrun stapler validate "$path"
}
