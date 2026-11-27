#!/usr/bin/env bash
# Notarise and staple a signed macOS client bundle, or check that the
# credential for it is in place.
#
# Usage:
#   notarize_client.sh <path/to/forge.app>
#   notarize_client.sh --check [<path/to/profile>]
#
# The app must already carry the signature that ships: signing it again after
# notarisation invalidates the ticket. `client-release` embeds the provisioning
# profile and signs again, then runs this. `--check` runs before the build, so
# a missing credential stops the release before it spends the build; the team
# id there comes from the provisioning profile, where the notarising run reads
# it from the signed bundle.
#
# The credential is never in this repo. `APPLE_ID` and `APPLE_PASSWORD` come
# from the environment, else from `~/.tauri/forge-notary.properties` (the same
# names, one per line). `APPLE_PASSWORD` is an app-specific password minted at
# appleid.apple.com, not the account password. `APPLE_TEAM_ID` follows the same
# two routes and falls back to the signature's own team id.
set -euo pipefail

notary_file="${FORGE_NOTARY_PROPERTIES:-$HOME/.tauri/forge-notary.properties}"

usage() {
    echo "usage: notarize_client.sh <path/to/forge.app>" >&2
    echo "       notarize_client.sh --check [<path/to/profile>]" >&2
    exit 2
}

# The first `KEY=value` line in the credential file, value taken whole: an
# app-specific password carries hyphens and can carry '='.
property() {
    sed -n "s/^$1=//p" "$notary_file" 2>/dev/null | head -1 || true
}

profile_team_id() {
    openssl smime -inform der -verify -noverify -in "$1" 2>/dev/null \
        | python3 -c 'import plistlib,sys; print(plistlib.loads(sys.stdin.buffer.read())["TeamIdentifier"][0])' \
            2>/dev/null || true
}

app_team_id() {
    codesign -dvvv "$1" 2>&1 | sed -n 's/^TeamIdentifier=//p' | head -1 || true
}

json_field() {
    python3 -c 'import json,sys; print(json.load(sys.stdin).get(sys.argv[1], ""))' "$1" \
        2>/dev/null || true
}

mode=notarise
app=""
profile=""
case "${1:-}" in
    --check)
        mode=check
        profile="${2:-}"
        if [ -n "$profile" ] && [ ! -f "$profile" ]; then
            echo "[ERROR] no provisioning profile at $profile" >&2
            exit 1
        fi
        ;;
    "" | -h | --help)
        usage
        ;;
    *)
        app="$1"
        if [ ! -d "$app" ]; then
            echo "[ERROR] no app bundle at $app" >&2
            exit 1
        fi
        ;;
esac

apple_id="${APPLE_ID:-$(property APPLE_ID)}"
apple_password="${APPLE_PASSWORD:-$(property APPLE_PASSWORD)}"
apple_team_id="${APPLE_TEAM_ID:-$(property APPLE_TEAM_ID)}"
if [ -z "$apple_team_id" ]; then
    if [ "$mode" = check ] && [ -n "$profile" ]; then
        apple_team_id=$(profile_team_id "$profile")
    elif [ "$mode" = notarise ]; then
        apple_team_id=$(app_team_id "$app")
    fi
fi

missing=()
[ -n "$apple_id" ] || missing+=(APPLE_ID)
[ -n "$apple_password" ] || missing+=(APPLE_PASSWORD)
[ -n "$apple_team_id" ] || missing+=(APPLE_TEAM_ID)
if [ "${#missing[@]}" -gt 0 ]; then
    echo "[ERROR] the notarisation credential is missing ${missing[*]}" >&2
    echo "        set them in the environment or in $notary_file," >&2
    echo "        one KEY=value per line (APPLE_PASSWORD is an app-specific password)" >&2
    echo "        (client/README.md, Notarisation)" >&2
    exit 1
fi

if [ "$mode" = check ]; then
    echo "[OK] notarisation credential present for $apple_id (team $apple_team_id)"
    exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
bundle=$(basename "$app")
zip="$tmp/$bundle.zip"
ditto -c -k --keepParent "$app" "$zip"

if ! result=$(xcrun notarytool submit "$zip" \
        --apple-id "$apple_id" --password "$apple_password" --team-id "$apple_team_id" \
        --wait --output-format json); then
    echo "[ERROR] notarytool could not submit $bundle" >&2
    exit 1
fi
submission=$(printf '%s' "$result" | json_field id)
status=$(printf '%s' "$result" | json_field status)
if [ "$status" != "Accepted" ]; then
    echo "[ERROR] notarisation was not accepted: status ${status:-unknown}${submission:+, submission $submission}" >&2
    if [ -n "$submission" ]; then
        xcrun notarytool log "$submission" \
            --apple-id "$apple_id" --password "$apple_password" --team-id "$apple_team_id" \
            >&2 || true
    fi
    exit 1
fi

# The service's own JSON and the submission id go to the release log, so the
# record names what Apple accepted rather than only that this script thought
# it did.
printf '%s\n' "$result"
echo "[OK] notarisation submission $submission accepted"

# The ticket is stapled before the caller packs anything from the bundle:
# the staple lives in the bundle, so a tarball packed from the app before
# this point carries a notarised signature with no ticket beside it.
if ! xcrun stapler staple "$app"; then
    echo "[ERROR] the notarisation ticket could not be stapled to $bundle" >&2
    exit 1
fi
if ! xcrun stapler validate "$app"; then
    echo "[ERROR] the stapled $bundle does not validate" >&2
    exit 1
fi
if ! spctl --assess --type execute --verbose=4 "$app"; then
    echo "[ERROR] Gatekeeper does not accept $bundle" >&2
    exit 1
fi
echo "[OK] notarised and stapled $bundle"
