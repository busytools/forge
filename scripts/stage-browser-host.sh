#!/usr/bin/env bash
# Stage the server's CEF host as a signed app bundle.
#
# **The server's own bundle, never the client's.** forge-server spawns this
# executable per profile; nothing in it comes from a client install. The
# layout is CEF's own macOS rule - the framework in Contents/Frameworks and
# five helper apps beside it, each re-entering this same executable - so
# Chromium's subprocesses work with no Tauri and no client app anywhere.
#
# Usage: stage-browser-host.sh
#
# Env overrides:
#   FORGE_BROWSER_BIN       the built host executable
#                           (default: <target>/release/forge-browser)
#   CEF_DIR                 the unpacked CEF distribution
#                           (default: discovered under the cargo target dir)
#   CARGO_TARGET_DIR        the cargo target dir to discover CEF under
#   FORGE_BROWSER_HOST_DIR  where the bundle is installed
#                           (default: ~/Library/Application Support/forge/browser-host)
set -euo pipefail

if [ "$(uname -s)" != "Darwin" ]; then
    exit 0
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
INSTALL_DIR="${FORGE_BROWSER_HOST_DIR:-$HOME/Library/Application Support/forge/browser-host}"
BIN="${FORGE_BROWSER_BIN:-$TARGET/release/forge-browser}"
CEF_DIR="${CEF_DIR:-$(find "$TARGET" -type d -path '*/out/cef_macos_aarch64' -print -quit 2>/dev/null)}"
VERSION="$(grep -m1 '^version = ' "$ROOT/Cargo.toml" | cut -d'"' -f2)"

# CEF derives every helper's name from the main bundle's executable, so this
# name is the layout, not a label.
MAIN="Forge Browser"
IDENTIFIER="dev.vedhavyas.forge.browser-host"

if [ -z "$CEF_DIR" ] || [ ! -d "$CEF_DIR/Chromium Embedded Framework.framework" ]; then
    echo "[ERROR] the CEF distribution is not unpacked under $TARGET - build forge-browser first" >&2
    exit 1
fi
if [ ! -x "$BIN" ]; then
    echo "[ERROR] no browser host executable at $BIN - build forge-browser first" >&2
    exit 1
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

APP="$STAGE/$MAIN.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Frameworks"
cp "$BIN" "$APP/Contents/MacOS/$MAIN"
chmod +x "$APP/Contents/MacOS/$MAIN"
cp -R "$CEF_DIR/Chromium Embedded Framework.framework" "$APP/Contents/Frameworks/"

write_plist() {
    local name="$1" app="$2"
    printf '%s\n' \
        '<?xml version="1.0" encoding="UTF-8"?>' \
        '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
        '<plist version="1.0">' \
        '<dict>' \
        "	<key>CFBundleName</key>" "	<string>$name</string>" \
        "	<key>CFBundleIdentifier</key>" "	<string>$IDENTIFIER</string>" \
        "	<key>CFBundleDisplayName</key>" "	<string>$name</string>" \
        '	<key>CFBundleDevelopmentRegion</key>' '	<string>English</string>' \
        "	<key>CFBundleVersion</key>" "	<string>$VERSION</string>" \
        "	<key>CFBundleExecutable</key>" "	<string>$name</string>" \
        '	<key>CFBundleInfoDictionaryVersion</key>' '	<string>6.0</string>' \
        '	<key>CFBundlePackageType</key>' '	<string>APPL</string>' \
        '	<key>CFBundleSignature</key>' '	<string>????</string>' \
        "	<key>CFBundleShortVersionString</key>" "	<string>$VERSION</string>" \
        '	<key>LSFileQuarantineEnabled</key>' '	<true/>' \
        '	<key>LSMinimumSystemVersion</key>' '	<string>11.0</string>' \
        '	<key>LSUIElement</key>' '	<string>1</string>' \
        '	<key>NSSupportsAutomaticGraphicsSwitching</key>' '	<true/>' \
        '</dict>' \
        '</plist>' > "$app/Contents/Info.plist"
}

write_plist "$MAIN" "$APP"

for SUFFIX in "Helper (GPU)" "Helper (Renderer)" "Helper (Plugin)" "Helper (Alerts)" "Helper"; do
    NAME="$MAIN $SUFFIX"
    HELPER="$APP/Contents/Frameworks/$NAME.app"
    mkdir -p "$HELPER/Contents/MacOS"
    cp "$BIN" "$HELPER/Contents/MacOS/$NAME"
    chmod +x "$HELPER/Contents/MacOS/$NAME"
    write_plist "$NAME" "$HELPER"
done

# Nested code first, then the bundle that seals it. Ad-hoc, like the client's
# own bundle: this host is built on the machine it runs on.
sign() {
    codesign --force --deep --sign - "$1" >/dev/null 2>&1 || {
        echo "[ERROR] $2 could not be signed" >&2
        exit 3
    }
}
sign "$APP/Contents/Frameworks/Chromium Embedded Framework.framework" "the CEF framework"
for SUFFIX in "Helper (GPU)" "Helper (Renderer)" "Helper (Plugin)" "Helper (Alerts)" "Helper"; do
    sign "$APP/Contents/Frameworks/$MAIN $SUFFIX.app" "the helper app for '$SUFFIX'"
done
sign "$APP" "the browser host bundle"
if ! codesign --verify --deep --strict "$APP" >/dev/null 2>&1; then
    echo "[ERROR] the staged browser host bundle does not verify" >&2
    exit 3
fi

mkdir -p "$INSTALL_DIR"
BUNDLE="$INSTALL_DIR/$MAIN.app"
rm -rf "$BUNDLE"
cp -R "$APP" "$BUNDLE"

echo "[OK] browser host staged at $BUNDLE (version $VERSION)"
