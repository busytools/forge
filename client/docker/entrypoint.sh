#!/bin/sh
# Seed the served directory from the build that ships inside the image, then
# keep it at the published release. nginx runs in the foreground; the poller
# runs beside it, and neither ever restarts the other.
set -eu

seed="${FORGE_SEED_DIR:-/usr/share/forge-web}"
served="${FORGE_SERVED:-/srv/forge-web}"
version_file="${FORGE_VERSION_FILE:-/usr/share/forge-web-version}"

# **The seed is named for the release the image was built at**, so the
# poller's same-version skip matches on the very first boot and a fresh
# container does not fetch a build it already serves. An image built without
# a version (a pull-request build, which is never published) falls back to
# the unversioned name and polls once.
seed_name=dist-bundled
if [ -s "$version_file" ]; then
    seed_version=$(cat "$version_file")
    [ -z "$seed_version" ] || seed_name="dist-$seed_version"
fi

# An empty volume is the ordinary first start (and a bind mount is whatever
# the host had): seed it so the container serves immediately, before the
# poller has reached the manifest.
if [ ! -e "$served/current/index.html" ]; then
    echo "[..] seeding $served from the image's own build ($seed_name)"
    mkdir -p "$served/$seed_name"
    cp -a "$seed/." "$served/$seed_name/"
    ln -sfn "$seed_name" "$served/current"
fi

"${FORGE_POLLER:-/usr/local/bin/poller.sh}" &

exec "${FORGE_NGINX:-nginx}" -g 'daemon off;'
