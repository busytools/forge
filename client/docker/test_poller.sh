#!/usr/bin/env bash
# The poller against a local manifest: the swap, the same-version skip, the
# refusal when the archive does not match the manifest's sha256, and the
# keep-serving answer when nothing answers at all.
#
# The sha256 is the one check between a tampered or truncated download and
# every browser the image serves, so it gets a test that needs no docker and
# no network: python3 for the server, curl and the poller itself.
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

release="$scratch/release"
served="$scratch/served"
mkdir -p "$release/dist" "$served"
printf 'the web build, served\n' > "$release/dist/index.html"
tar -C "$release/dist" -czf "$release/web.tar.gz" .
sum=$(sha_of "$release/web.tar.gz")

port=18099
python3 -m http.server "$port" --directory "$release" > /dev/null 2>&1 &
server=$!

manifest() {
    version="$1"; digest="$2"
    cat > "$release/latest.json" <<JSON
{
  "version": "$version",
  "pub_date": "2026-10-06T00:00:00Z",
  "platforms": {},
  "android": { "url": "https://example.invalid/app.apk" },
  "web": {
    "version": "$version",
    "url": "http://127.0.0.1:$port/web.tar.gz",
    "sha256": "$digest"
  }
}
JSON
}

poll() {
    FORGE_SERVED="$served" \
        FORGE_LATEST_URL="$1" \
        FORGE_POLL_ONCE=1 \
        sh "$here/poller.sh"
}

manifest "9.9.9" "$sum"
url="http://127.0.0.1:$port/latest.json"

# The server needs a moment before it answers; a poll that ran into that
# would read a refusal as an unreachable manifest and correctly do nothing.
for _ in $(seq 50); do
    curl -fsS "$url" > /dev/null 2>&1 && break
    sleep 0.2
done

poll "$url" > /dev/null 2>&1 || fail "the first poll failed"
[ "$(readlink "$served/current")" = "dist-9.9.9" ] || fail "the swap did not land"
[ -e "$served/current/index.html" ] || fail "the served build has no index"
[ -e "$served/latest.json" ] || fail "the manifest was not kept beside the build"
echo "[OK] a new release is fetched, checked and served"

# The same-version skip: with the archive taken away the poll must still
# answer, because it should not have looked.
mv "$release/web.tar.gz" "$scratch/web.tar.gz"
poll "$url" > /dev/null 2>&1 || fail "the same-version poll failed"
[ "$(readlink "$served/current")" = "dist-9.9.9" ] || fail "the same-version poll moved the build"
mv "$scratch/web.tar.gz" "$release/web.tar.gz"
echo "[OK] a poll that names the served version fetches nothing"

# The integrity check: a manifest whose sha256 does not match the archive
# must refuse the release and leave the served build alone.
manifest "9.9.10" "0000000000000000000000000000000000000000000000000000000000000000"
if poll "$url" > /dev/null 2>&1; then
    fail "a mismatched sha256 was accepted"
fi
[ "$(readlink "$served/current")" = "dist-9.9.9" ] || fail "a refused release still moved the build"
[ ! -e "$served/dist-9.9.10" ] || fail "a refused release was extracted"
echo "[OK] a mismatched sha256 refuses the release and keeps the served build"

# Nothing answering is not fatal: the container keeps serving what it has.
if ! poll "http://127.0.0.1:1/latest.json" > /dev/null 2>&1; then
    fail "an unreachable manifest was treated as fatal"
fi
[ "$(readlink "$served/current")" = "dist-9.9.9" ] || fail "an unreachable manifest moved the build"
echo "[OK] an unreachable manifest keeps the served build"
