#!/usr/bin/env bash
# scripts/vendor_browser_stack.sh - fetch, verify and unpack the driver
# stack the client bundles inside the app: node and @playwright/mcp. The
# browser itself is the machine's own - the host drives the installed Brave
# or Google Chrome - so nothing here fetches one.
#
# Idempotent at the pins below: anything already there at its pinned version
# is left alone. `just client-tauri-bundle` and `just client-release` run
# this before they build, so a release never ships without it, and the pins
# are printed either way so a release log names the stack it shipped.
#
# Where each hash comes from, because they are not both the same kind:
#
#   node    - nodejs.org publishes SHASUMS256.txt per release; the value
#             below is that file's line for the pinned platform's tarball.
#   mcp     - npm verifies every tarball against the registry's own
#             integrity, so the pin is the version itself.
#             `--ignore-scripts` keeps playwright's installer from fetching
#             a browser the host does not use.
#
# Usage:
#   scripts/vendor_browser_stack.sh
#
# Env overrides:
#   FORGE_BROWSER_STACK_DIR   where the stack lands (default: the client's
#                             own browser-stack/ directory, which is
#                             gitignored)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"

NODE_VERSION="v24.21.0"
NODE_SHA256="bed7eea5325e1108f32ce5228ddd6a5f0f08a499ee42aa7442aea583702f6057"
PLAYWRIGHT_MCP_VERSION="0.0.83"

STACK="${FORGE_BROWSER_STACK_DIR:-$REPO/client/src-tauri/browser-stack}"

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)
        NODE_ARCH="darwin-arm64"
        ;;
    Darwin-x86_64)
        echo "[ERROR] only Apple Silicon is vendored so far: the x64 artifacts carry their" >&2
        echo "        own hashes, and nothing here has measured or pinned them." >&2
        exit 1
        ;;
    *)
        echo "[ERROR] the driver stack is vendored for macOS only: this is $(uname -s)-$(uname -m)." >&2
        echo "        Windows is parked (issue #1774); Android uses the system WebView." >&2
        exit 1
        ;;
esac

need() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "[ERROR] $1 is required and not on PATH" >&2
        exit 1
    }
}
need curl
need npm
need shasum
need tar

WORK="$(mktemp -d "${TMPDIR:-/tmp}/forge-browser-stack.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

# Fail on bytes that are not the pinned ones. A partial download and a
# tampered one look the same here, and both must not reach the bundle.
verify_sha256() {
    local file="$1" want="$2" what="$3"
    local got
    got="$(shasum -a 256 "$file" | awk '{print $1}')"
    if [ "$got" != "$want" ]; then
        echo "[ERROR] $what does not hash to its pin:" >&2
        echo "        want $want" >&2
        echo "        got  $got" >&2
        exit 1
    fi
}

fetch() {
    local url="$1" out="$2"
    echo "[..] fetching $url"
    curl -fSL --retry 3 -o "$out" "$url"
}

# --- node: bin/node alone -------------------------------------------------

node_bin="$STACK/node/bin/node"
if [ -x "$node_bin" ] && [ "$("$node_bin" --version 2>/dev/null || true)" = "$NODE_VERSION" ]; then
    echo "[..] node $NODE_VERSION is already vendored"
else
    tarball="node-$NODE_VERSION-$NODE_ARCH.tar.gz"
    fetch "https://nodejs.org/dist/$NODE_VERSION/$tarball" "$WORK/$tarball"
    verify_sha256 "$WORK/$tarball" "$NODE_SHA256" "node $NODE_VERSION"
    # `bin/node` alone: the rest of the tree is npm and headers, which
    # nothing here needs.
    tar -xzf "$WORK/$tarball" -C "$WORK" "node-$NODE_VERSION-$NODE_ARCH/bin/node"
    mkdir -p "$STACK/node/bin"
    install -m 755 "$WORK/node-$NODE_VERSION-$NODE_ARCH/bin/node" "$node_bin"
fi

# --- @playwright/mcp: the driver, with no browser of its own --------------

mcp_dir="$STACK/playwright-mcp"
mcp_manifest="$mcp_dir/node_modules/@playwright/mcp/package.json"
if [ -f "$mcp_manifest" ] && [ "$(node -p "require('$mcp_manifest').version" 2>/dev/null || true)" = "$PLAYWRIGHT_MCP_VERSION" ]; then
    echo "[..] @playwright/mcp $PLAYWRIGHT_MCP_VERSION is already vendored"
else
    mkdir -p "$mcp_dir"
    printf '{\n  "private": true,\n  "dependencies": { "@playwright/mcp": "%s" }\n}\n' \
        "$PLAYWRIGHT_MCP_VERSION" > "$mcp_dir/package.json"
    echo "[..] installing @playwright/mcp $PLAYWRIGHT_MCP_VERSION"
    npm install --prefix "$mcp_dir" --no-audit --no-fund --ignore-scripts --silent
    got="$(node -p "require('$mcp_manifest').version")"
    if [ "$got" != "$PLAYWRIGHT_MCP_VERSION" ]; then
        echo "[ERROR] @playwright/mcp installed as $got, expected $PLAYWRIGHT_MCP_VERSION" >&2
        exit 1
    fi
fi

printf 'node %s %s\n@playwright/mcp %s (npm integrity)\n' \
    "$NODE_VERSION" "$NODE_SHA256" "$PLAYWRIGHT_MCP_VERSION" \
    > "$STACK/manifest.txt"

node_size="$(du -sh "$STACK/node" 2>/dev/null | awk '{print $1}')"
mcp_size="$(du -sh "$STACK/playwright-mcp" 2>/dev/null | awk '{print $1}')"

echo "[OK] driver stack at $STACK"
echo "     node $NODE_VERSION ($node_size)"
echo "     @playwright/mcp $PLAYWRIGHT_MCP_VERSION ($mcp_size)"
