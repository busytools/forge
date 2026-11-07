#!/usr/bin/env bash
# Refuse to build an install from anything but main.
#
# `just release` publishes whatever the checkout holds, and `just install` /
# the three client recipes are the build steps it calls. A checkout behind
# origin/main therefore builds yesterday's code and every artefact says today's
# version, with nothing in the build output disagreeing. That happened on
# 2026-11-07: five merged fixes were "installed" from the release commit
# because the local main was five commits stale.
#
# So: fast-forward a clean main (the common case needs no ceremony), and refuse
# everything else - a dirty tree, another branch, or a main that cannot
# fast-forward. A commit carrying a tag is allowed, because `just release`
# builds the version commit after tagging it and before pushing it.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

if [ -n "$(git status --porcelain)" ]; then
    echo "[ERROR] working tree dirty - commit or stash before building an install" >&2
    exit 1
fi

branch=$(git rev-parse --abbrev-ref HEAD)
if [ "$branch" != "main" ]; then
    echo "[ERROR] on '$branch', not main - installs are built from main" >&2
    exit 1
fi

git fetch --quiet origin main

if [ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]; then
    if tag=$(git describe --exact-match --tags HEAD 2>/dev/null); then
        echo "[OK] building from $(git rev-parse --short HEAD) ($tag), the commit this release tagged"
        exit 0
    fi
    if ! git merge --ff-only --quiet origin/main; then
        echo "[ERROR] local main cannot fast-forward to origin/main (HEAD $(git rev-parse --short HEAD), origin/main $(git rev-parse --short origin/main)) - resolve it before building an install" >&2
        exit 1
    fi
    echo "[OK] fast-forwarded main to $(git rev-parse --short HEAD)"
fi

echo "[OK] building from $(git rev-parse --short HEAD): $(git log -1 --pretty=%s)"
