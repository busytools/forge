#!/usr/bin/env bash
# The image, run: the app is served, and every asset says `no-cache` and
# carries an ETag. Without those two a swapped build hides behind whatever
# cache is in the path, which is the failure the standing rule names.
#
# Needs docker; the image workflow builds the image and runs this against it.
set -euo pipefail

image="${1:?usage: test_serving.sh <image>}"
port=18080

container=$(docker run -d --rm -p "$port:8080" "$image")
cleanup() {
    docker stop "$container" > /dev/null 2>&1 || true
}
trap cleanup EXIT

echo "[..] waiting for the server"
ready=""
for _ in $(seq 30); do
    if curl -fsS "http://127.0.0.1:$port/" > /dev/null 2>&1; then
        ready=yes
        break
    fi
    sleep 1
done
[ -n "$ready" ] || { docker logs "$container" >&2; echo "[ERROR] the server never answered" >&2; exit 1; }

head=$(curl -fsSI "http://127.0.0.1:$port/index.html")
grep -qi '^etag:' <<< "$head" || { echo "[ERROR] index.html carries no ETag" >&2; exit 1; }
grep -qi '^cache-control: *no-cache' <<< "$head" || {
    echo "[ERROR] index.html is cacheable" >&2
    exit 1
}
echo "[OK] index.html says no-cache and carries an ETag"

# The app's own routing answers too, rather than 404ing on a real URL.
status=$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/session/Some/Project/lead")
[ "$status" = "200" ] || { echo "[ERROR] a session URL answered $status" >&2; exit 1; }
echo "[OK] the app's own routes answer"
