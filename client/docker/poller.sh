#!/bin/sh
# Keep the served build at the release the manifest names.
#
# One manifest serves every half, and this reads its `web` block: the release
# version, the archive's URL, and the sha256 that is both the integrity check
# and the same-version skip. The archive is verified before anything is
# extracted, the extraction lands in its own directory, and the swap is a
# symlink flip - so a request in flight is never served half a build, and
# nginx is never restarted.
#
# A failed poll is not fatal: the container keeps serving what it has and
# tries again, so a network blip is a delayed update rather than a dead page.
set -eu

served="${FORGE_SERVED:-/srv/forge-web}"
latest_url="${FORGE_LATEST_URL:-https://github.com/busytools/forge/releases/latest/download/latest.json}"
interval="${FORGE_POLL_SECONDS:-3600}"

fetch() {
    curl -fsSL --max-time 60 --retry 2 "$1"
}

poll_once() {
    manifest=$(fetch "$latest_url") || {
        echo "[..] the manifest is not reachable"
        return 0
    }
    version=$(printf '%s' "$manifest" | jq -r '.web.version // empty')
    url=$(printf '%s' "$manifest" | jq -r '.web.url // empty')
    sum=$(printf '%s' "$manifest" | jq -r '.web.sha256 // empty')
    if [ -z "$version" ] || [ -z "$url" ] || [ -z "$sum" ]; then
        echo "[..] the manifest carries no web block"
        return 0
    fi

    current=$(basename "$(readlink "$served/current" 2>/dev/null || echo none)")
    if [ "$current" = "dist-$version" ]; then
        return 0
    fi

    staging=$(mktemp -d)
    if ! fetch "$url" > "$staging/web.tar.gz"; then
        echo "[..] the web archive is not reachable"
        rm -rf "$staging"
        return 0
    fi
    if ! echo "$sum  $staging/web.tar.gz" | sha256sum -c - > /dev/null 2>&1; then
        echo "[ERROR] the web archive does not match the manifest's sha256"
        rm -rf "$staging"
        return 1
    fi
    mkdir -p "$staging/dist"
    tar -xzf "$staging/web.tar.gz" -C "$staging/dist"
    rm -rf "${served:?}/dist-$version"
    mv "$staging/dist" "$served/dist-$version"
    # The flip, as one rename: a plain `ln -sf` unlinks first, and a request
    # in that instant would find no build at all. BSD mv has no -T, so the
    # fallback is what a mac (where test_poller.sh runs) takes.
    ln -sfn "dist-$version" "$served/current.new"
    if ! mv -Tf "$served/current.new" "$served/current" 2>/dev/null; then
        ln -sfn "dist-$version" "$served/current"
        rm -f "$served/current.new"
    fi
    printf '%s' "$manifest" > "$served/latest.json"
    # Everything older than the release being served, the seed included.
    for dir in "$served"/dist-*; do
        [ "$dir" = "$served/dist-$version" ] || rm -rf "$dir"
    done
    rm -rf "$staging"
    echo "[OK] the web build is now $version"
}

if [ -n "${FORGE_POLL_ONCE:-}" ]; then
    poll_once
    exit $?
fi

while true; do
    poll_once || echo "[..] a poll failed; keeping what is served and trying again"
    sleep "$interval"
done
