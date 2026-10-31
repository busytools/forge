#!/usr/bin/env bash
# The entrypoint's seed naming, outside the image: a release-built image names
# the seed `dist-<version>` so the poller's same-version skip matches on the
# very first boot, and a pull-request image (no version baked) keeps the
# unversioned name and polls once.
#
# The script runs the real entrypoint with its container paths overridden and
# a no-op nginx/poller, so the seeding logic is the shipped one.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

seed="$scratch/seed"
served="$scratch/served"
mkdir -p "$seed"
printf 'the build\n' > "$seed/index.html"
printf '1.2.3' > "$scratch/version"

fail() {
    echo "[ERROR] $1" >&2
    exit 1
}

run() {
    FORGE_SEED_DIR="$seed" \
        FORGE_SERVED="$served" \
        FORGE_VERSION_FILE="$1" \
        FORGE_POLLER=true \
        FORGE_NGINX=true \
        sh "$here/entrypoint.sh" > /dev/null
}

# A pull-request image bakes no version: the seed keeps the unversioned name.
run "$scratch/no-version-file"
[ "$(basename "$(readlink "$served/current")")" = "dist-bundled" ] \
    || fail "an image without a version did not seed under dist-bundled"
[ -f "$served/dist-bundled/index.html" ] || fail "the unversioned seed carries no index"

# A release image names the seed for its version, which is what the poller's
# skip compares against on first boot.
rm -rf "$served"
run "$scratch/version"
[ "$(basename "$(readlink "$served/current")")" = "dist-1.2.3" ] \
    || fail "a release image did not seed under its version"
[ -f "$served/dist-1.2.3/index.html" ] || fail "the versioned seed carries no index"

# A second start on a served volume does not re-seed.
printf 'changed\n' > "$seed/index.html"
run "$scratch/version"
[ "$(cat "$served/dist-1.2.3/index.html")" = "the build" ] \
    || fail "the entrypoint re-seeded a volume that already serves"

echo "[OK] the entrypoint names the seed for the image's release and seeds once"
