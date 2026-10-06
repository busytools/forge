# The web client's image: the built bundle, served from a volume the poller
# keeps at the published release. There is no server half here - the pages
# connect to a forge someone else runs.
#
# The build context is `client/`, so `dist/` must exist before this builds;
# the workflow runs `npm run build` first.
#
# The base is the unprivileged nginx: it already runs as 101 and listens on
# 8080, which are the two facts hub-modules' compose is written against.
# The digest pins the index the tag pointed at when this was written
# (2026-10-06) - the tag itself moves along the minor line. To bump: read the
# tag's `digest` from
# https://hub.docker.com/v2/repositories/nginxinc/nginx-unprivileged/tags/<tag>
# and replace both halves below.
FROM nginxinc/nginx-unprivileged:1.31-alpine3.24@sha256:b9241c6e7b8e9a862f129d8d4199ab64b10390949a78bdd5603379b32c844083

# curl fetches and jq reads the manifest; that is the poller's whole
# dependency list. `--no-cache` keeps the index out of the layer.
USER root
RUN apk add --no-cache curl jq

COPY docker/nginx.conf /etc/nginx/conf.d/default.conf
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
COPY docker/poller.sh /usr/local/bin/poller.sh
RUN chmod +x /usr/local/bin/entrypoint.sh /usr/local/bin/poller.sh

# The build that ships inside the image: an empty volume is seeded from it,
# so a fresh container serves before it has ever reached the manifest.
COPY dist /usr/share/forge-web

RUN mkdir -p /srv/forge-web && chown -R 101:101 /srv/forge-web

USER 101
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
