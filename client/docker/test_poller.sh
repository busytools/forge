#!/usr/bin/env bash
# The poller against a local manifest: the swap and the prune, the
# same-version skip, the refusal when the archive does not match the
# manifest's sha256, and the keep-serving answer when nothing answers at all.
#
# The sha256 is the one check between a tampered or truncated download and
# every browser the image serves, and the skip is what keeps a matching poll
# from re-extracting the directory being served, so both get a test that
# needs no docker and no network: python3 for the server, curl and the
# poller itself.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
scratch=$(mktemp -d)
server=""
cleanup() {
    rm -rf "$scratch"
    [ -n "$server" ] && kill "$server" 2>/dev/null || true
}
trap cleanup EXIT

sha_of() {
    if command -v sha256sum > /dev/null; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

fail() {
    echo "[ERROR] $1" >&2
    exit 1
}

served_of() {
    readlink "$served/current" 2>/dev/null || echo none
}

fetches() {
    grep -c 'GET /web.tar.gz' "$scratch/server.log" 2>/dev/null || true
}

release="$scratch/release"
served="$scratch/served"
mkdir -p "$release/dist" "$served"
printf 'the web build, served\n' > "$release/dist/index.html"
tar -C "$release/dist" -czf "$release/web.tar.gz" .
sum=$(sha_of "$release/web.tar.gz")

port=18099
python3 -m http.server "$port" --directory "$release" > "$scratch/server.log" 2>&1 &
server=$!

manifest() {
    version="$1"; digest="$2"; body="$3"
    cat > "$release/latest.json" <<JSON
{
  "version": "$version",
  "pub_date": "2026-10-06T00:00:00Z",
  "platforms": {},
  "android": { "url": "https://example.invalid/app.apk" }$body
}
JSON
}

web_block() {
    printf ',\n  "web": {\n    "version": "%s",\n    "url": "http://127.0.0.1:%s/web.tar.gz",\n    "sha256": "%s"\n  }' "$1" "$port" "$2"
}

poll() {
    FORGE_SERVED="$served" \
        FORGE_LATEST_URL="$1" \
        FORGE_POLL_ONCE=1 \
        sh "$here/poller.sh"
}

manifest "9.9.9" "$sum" "$(web_block 9.9.9 "$sum")"
url="http://127.0.0.1:$port/latest.json"

# The server needs a moment before it answers; a poll that ran into that
# would read a refusal as an unreachable manifest and correctly do nothing.
for _ in $(seq 50); do
    curl -fsS "$url" > /dev/null 2>&1 && break
    sleep 0.2
done

# A stale build beside the one being served, which the swap prunes.
mkdir -p "$served/dist-9.9.8"
[ -e "$served/dist-9.9.8" ] || fail "the decoy was not made"

poll "$url" > /dev/null 2>&1 || fail "the first poll failed"
[ "$(served_of)" = "dist-9.9.9" ] || fail "the swap did not land"
[ -e "$served/current/index.html" ] || fail "the served build has no index"
[ -e "$served/latest.json" ] || fail "the manifest was not kept beside the build"
[ ! -e "$served/dist-9.9.8" ] || fail "the stale build was not pruned"
[ "$(fetches)" -ge 1 ] || fail "the archive was never fetched"
echo "[OK] a new release is fetched, checked, served, and the stale build pruned"

# The same-version skip: the poll must not fetch at all, because a fetch is
# followed by a re-extract of the directory `current` points at - a window
# with nothing served in it.
before=$(fetches)
poll "$url" > /dev/null 2>&1 || fail "the same-version poll failed"
[ "$(fetches)" = "$before" ] || fail "the same-version poll fetched the archive again"
[ "$(served_of)" = "dist-9.9.9" ] || fail "the same-version poll moved the build"
echo "[OK] a poll that names the served version fetches nothing"

# A manifest with no web block is not an update: the poll answers and does
# nothing, which is what a desktop-only release looks like.
manifest "9.9.10" "$sum" ""
poll "$url" > /dev/null 2>&1 || fail "a manifest with no web block was fatal"
[ "$(served_of)" = "dist-9.9.9" ] || fail "a manifest with no web block moved the build"
echo "[OK] a manifest with no web block is answered and does nothing"

# The integrity check: a manifest whose sha256 does not match the archive
# must refuse the release and leave the served build alone.
manifest "9.9.10" "0000000000000000000000000000000000000000000000000000000000000000" \
    "$(web_block 9.9.10 "0000000000000000000000000000000000000000000000000000000000000000")"
if poll "$url" > /dev/null 2>&1; then
    fail "a mismatched sha256 was accepted"
fi
[ "$(served_of)" = "dist-9.9.9" ] || fail "a refused release still moved the build"
[ ! -e "$served/dist-9.9.10" ] || fail "a refused release was extracted"
echo "[OK] a mismatched sha256 refuses the release and keeps the served build"

# Nothing answering is not fatal: the container keeps serving what it has.
if ! poll "http://127.0.0.1:1/latest.json" > /dev/null 2>&1; then
    fail "an unreachable manifest was treated as fatal"
fi
[ "$(served_of)" = "dist-9.9.9" ] || fail "an unreachable manifest moved the build"
echo "[OK] an unreachable manifest keeps the served build"
