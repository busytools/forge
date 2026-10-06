#!/usr/bin/env bash
# The image, run, against a volume of its own: the app is served from the
# build seeded out of the image, every asset says `no-cache` and carries an
# ETag (without them a swapped build hides behind whatever cache is in the
# path), the manifest is reachable at its own path, the app's routes answer,
# and a second start on the same volume serves the same way.
#
# The poller is pinned to a dead manifest URL so this is deterministic -
# what is under test is the image serving, not whatever the network happens
# to answer at the moment.
set -euo pipefail

image="${1:?usage: test_serving.sh <image>}"
port=18080
served=$(mktemp -d)
# The container runs as 101 and a bind mount keeps the host's ownership, so
# the volume it will seed and write has to be writable by it.
chmod 777 "$served"
manifest='{"version":"9.9.9","platforms":{},"web":{"version":"9.9.9","url":"http://127.0.0.1:1/none.tar.gz","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}}'
printf '%s' "$manifest" > "$served/latest.json"

container=""
cleanup() {
    [ -n "$container" ] && docker stop "$container" > /dev/null 2>&1 || true
    # The container wrote into the volume as uid 101, so the runner outside
    # may not be able to remove it; a plain `rm` first keeps a local run
    # from needing sudo at all.
    rm -rf "$served" 2> /dev/null || sudo rm -rf "$served" 2> /dev/null || true
}
trap cleanup EXIT

start() {
    container=$(docker run -d --rm -p "$port:8080" \
        -v "$served:/srv/forge-web" \
        -e FORGE_LATEST_URL=http://127.0.0.1:1/none.json \
        "$image")
}

wait_for() {
    for _ in $(seq 30); do
        if curl -fsS "http://127.0.0.1:$port/" > /dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    docker logs "$container" >&2
    echo "[ERROR] $1" >&2
    exit 1
}

start
echo "[..] waiting for the server"
wait_for "the server never answered"

head=$(curl -fsSI "http://127.0.0.1:$port/index.html")
grep -qi '^etag:' <<< "$head" || { echo "[ERROR] index.html carries no ETag" >&2; exit 1; }
grep -qi '^cache-control: *no-cache' <<< "$head" || {
    echo "[ERROR] index.html is cacheable" >&2
    exit 1
}
echo "[OK] index.html says no-cache and carries an ETag"

# The manifest sits beside the served build rather than inside it, so its
# location names the file; a root-relative lookup would 404 and the page
# would never draw the latest.
mhead=$(curl -fsSI "http://127.0.0.1:$port/latest.json")
grep -qi '^cache-control: *no-cache' <<< "$mhead" || {
    echo "[ERROR] the manifest is cacheable" >&2
    exit 1
}
body=$(curl -fsS "http://127.0.0.1:$port/latest.json")
[ "$body" = "$manifest" ] || { echo "[ERROR] /latest.json did not answer the manifest" >&2; exit 1; }
echo "[OK] /latest.json answers the manifest, no-cache"

status=$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/session/Some/Project/lead")
[ "$status" = "200" ] || { echo "[ERROR] a session URL answered $status" >&2; exit 1; }
echo "[OK] the app's own routes answer"

# A second start on the same volume: the seed is skipped, the build and the
# manifest are the ones already there.
docker stop "$container" > /dev/null
container=""
start
wait_for "the restarted server never answered"
[ "$(curl -fsS "http://127.0.0.1:$port/latest.json")" = "$manifest" ] || {
    echo "[ERROR] the restarted container lost the volume" >&2
    exit 1
}
echo "[OK] a second start on the same volume serves the same way"
