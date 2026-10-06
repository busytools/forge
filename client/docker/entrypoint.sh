#!/bin/sh
# Seed the served directory from the build that ships inside the image, then
# keep it at the published release. nginx runs in the foreground; the poller
# runs beside it, and neither ever restarts the other.
set -eu

seed=/usr/share/forge-web
served="${FORGE_SERVED:-/srv/forge-web}"

# An empty volume is the ordinary first start (and a bind mount is whatever
# the host had): seed it so the container serves immediately, before the
# poller has reached the manifest.
if [ ! -e "$served/current/index.html" ]; then
    echo "[..] seeding $served from the image's own build"
    mkdir -p "$served/dist-bundled"
    cp -a "$seed/." "$served/dist-bundled/"
    ln -sfn dist-bundled "$served/current"
fi

/usr/local/bin/poller.sh &

exec nginx -g 'daemon off;'
