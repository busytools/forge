#!/usr/bin/env bash
# scripts/vendor_browser_stack.sh - fetch, verify and unpack the driver
# stack the client bundles inside the app: node and @playwright/mcp for the
# desktop, libnode for Android. The browser itself is the machine's own on
# the desktop (Brave or Google Chrome) and the system WebView on Android, so
# nothing here fetches one.
#
# Idempotent at the pins below: anything already there at its pinned version
# is left alone. `just client-tauri-bundle` and `just client-release` run
# this before they build, so a release never ships without it, and the pins
# are printed either way so a release log names the stack it shipped.
#
# Where each hash comes from, because they are not all the same kind:
#
#   node    - nodejs.org publishes SHASUMS256.txt per release; the value
#             below is that file's line for the pinned platform's tarball.
#   mcp     - npm verifies every tarball against the registry's own
#             integrity, so the pin is the version itself.
#             `--ignore-scripts` keeps playwright's installer from fetching
#             a browser the host does not use.
#   libnode - the gmaclennan/nodejs-mobile Android zip, hashed by this repo
#             when the pin was first vendored and verified against the
#             release's artifact; the fork's own build is a patches recipe
#             over a pinned upstream Node tag (see the issue's vetting note).
#
# Usage:
#   scripts/vendor_browser_stack.sh            # desktop: node + driver
#   scripts/vendor_browser_stack.sh --android  # android: libnode + headers
#
# Env overrides:
#   FORGE_BROWSER_STACK_DIR   where the desktop stack lands (default: the
#                             client's own browser-stack/ directory, which
#                             is gitignored)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"

NODE_VERSION="v24.21.0"
NODE_SHA256="bed7eea5325e1108f32ce5228ddd6a5f0f08a499ee42aa7442aea583702f6057"
PLAYWRIGHT_MCP_VERSION="0.0.83"
# The Android engine: gmaclennan/nodejs-mobile, the maintained Node 24 line
# (the nodejs-mobile org fork is dead at 18.20.4). One ABI: what the client
# ships.
LIBNODE_VERSION="24.18.0-0"
LIBNODE_SHA256="ceb86b0b8130006195a60cd37393ebe0fd665b644ce8d5674dfba1da65d3be28"
LIBNODE_ABI="arm64-v8a"

ANDROID_MODE=false
if [ "${1:-}" = "--android" ]; then
    ANDROID_MODE=true
fi

STACK="${FORGE_BROWSER_STACK_DIR:-$REPO/client/src-tauri/browser-stack}"
ANDROID_DIR="$REPO/client/src-tauri/gen/android/app/src/main"

if [ "$ANDROID_MODE" = false ]; then
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
            echo "[ERROR] the desktop driver stack is vendored for macOS only: this is $(uname -s)-$(uname -m)." >&2
            echo "        Windows is parked (issue #1774); Android vendors with --android." >&2
            exit 1
            ;;
    esac
fi

need() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "[ERROR] $1 is required and not on PATH" >&2
        exit 1
    }
}
need curl
need shasum
if [ "$ANDROID_MODE" = true ]; then
    need unzip
else
    need npm
    need tar
fi

# **The generated copy is not pruned by anything.** Tauri's resource copy
# into gen/android's assets never deletes files that no longer exist in the
# source, and the Android packer ships whatever gen/ holds - so a stale
# `browser/` tree (a vendored Chromium from before the machine's browser
# switch) shipped a 516MB APK from a source tree that was already clean
# (measured 2026-10-08). The Android bundle carries ONLY the driver
# (playwright-mcp); the mac node binary and any vendored browser are dead
# weight there. Clear the non-driver subdirs at their source, both arms.
prune_generated_stack() {
    local gen="$REPO/client/src-tauri/gen/android/app/src/main/assets/browser-stack"
    [ -d "$gen" ] || return 0
    for stale in "$gen/browser" "$gen/node"; do
        if [ -d "$stale" ]; then
            echo "[..] removing the stale generated copy $(basename "$stale")"
            find "$stale" -depth -delete
        fi
    done
}

WORK="$(mktemp -d "${TMPDIR:-/tmp}/forge-browser-stack.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

# The browser is the machine's own now, and a `browser/` tree from when one
# was vendored is removed here: `bundle.resources` globs the whole stack, so
# a leftover would keep packing a Chromium nothing can launch into every
# release.
if [ -d "$STACK/browser" ]; then
    echo "[..] removing the stale vendored browser tree"
    find "$STACK/browser" -depth -delete
fi
prune_generated_stack

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

# --- android: libnode and its headers --------------------------------------
#
# The engine the in-app driver links against. `libnode.so` goes to jniLibs
# (the only place an app may dlopen from), the headers to the cpp tree the
# JNI shim compiles against. Both are gitignored; this script is what puts
# them there.

if [ "$ANDROID_MODE" = true ]; then
    need unzip
    prune_generated_stack
    zip_name="nodejs-mobile-android-$LIBNODE_VERSION.zip"
    lib_out="$ANDROID_DIR/jniLibs/$LIBNODE_ABI/libnode.so"
    inc_out="$ANDROID_DIR/cpp/nodejs-mobile/include"
    marker="$ANDROID_DIR/cpp/nodejs-mobile/manifest.txt"
    if [ -f "$lib_out" ] && [ -f "$marker" ] && grep -q "libnode $LIBNODE_VERSION" "$marker" 2>/dev/null; then
        echo "[..] libnode $LIBNODE_VERSION is already vendored"
    else
        fetch "https://github.com/gmaclennan/nodejs-mobile/releases/download/v$LIBNODE_VERSION/$zip_name" "$WORK/$zip_name"
        verify_sha256 "$WORK/$zip_name" "$LIBNODE_SHA256" "libnode $LIBNODE_VERSION"
        mkdir -p "$(dirname "$lib_out")" "$(dirname "$inc_out")"
        unzip -q -o "$WORK/$zip_name" -d "$WORK/njm" "bin/$LIBNODE_ABI/libnode.so"
        find "$inc_out" -depth -delete 2>/dev/null || true
        unzip -q -o "$WORK/$zip_name" -d "$WORK/njm" "include/*"
        mkdir -p "$inc_out"
        cp -R "$WORK/njm/include/node" "$inc_out/node"
        install -m 644 "$WORK/njm/bin/$LIBNODE_ABI/libnode.so" "$lib_out"
    fi

    # libnode.so links libc++_shared; the NDK's copy must ride beside it in
    # jniLibs (AGP packages the STL only for its own shared-STL bookkeeping,
    # and the shim selects the STL from CMake). Taken from the newest
    # installed NDK, the same one the android build resolves.
    sdk=""
    for candidate in "${ANDROID_HOME:-}" "${ANDROID_SDK_ROOT:-}" "$HOME/Library/Android/sdk" /opt/homebrew/share/android-commandlinetools; do
        [ -n "$candidate" ] || continue
        if [ -d "$candidate/ndk" ] || [ -d "$candidate/platforms" ]; then
            sdk="$candidate"
            break
        fi
    done
    ndk=""
    if [ -n "$sdk" ] && [ -d "$sdk/ndk" ]; then
        ndk="$(ls -1 "$sdk/ndk" | sort -V | tail -n1)"
        ndk="$sdk/ndk/$ndk"
    fi
    if [ -z "$ndk" ] || [ ! -d "$ndk" ]; then
        echo "[ERROR] no Android NDK found to take libc++_shared.so from - set ANDROID_HOME" >&2
        exit 1
    fi
    stl_src="$ndk/toolchains/llvm/prebuilt/darwin-x86_64/sysroot/usr/lib/aarch64-linux-android/libc++_shared.so"
    if [ ! -f "$stl_src" ]; then
        echo "[ERROR] libc++_shared.so not found under $ndk" >&2
        exit 1
    fi
    install -m 644 "$stl_src" "$(dirname "$lib_out")/libc++_shared.so"
    printf 'libnode %s %s (gmaclennan/nodejs-mobile, %s)\n' \
        "$LIBNODE_VERSION" "$LIBNODE_SHA256" "$LIBNODE_ABI" > "$marker"
    echo "[OK] android engine at $ANDROID_DIR"
    echo "     libnode $LIBNODE_VERSION ($LIBNODE_ABI, $(du -sh "$lib_out" | awk '{print $1}'))"
    exit 0
fi

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
