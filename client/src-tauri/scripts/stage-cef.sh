#!/usr/bin/env bash
# Stage the app's own Chromium where the macOS bundler copies it.
#
# **The bundler, not a step after it.** `bundle.macOS.files` in tauri.conf
# maps these into Contents/Frameworks while the bundler builds the app, so
# the ad-hoc signature, the update tarball and the dmg all carry the engine.
# Laying CEF in after `tauri build` left the tarball without it - the install
# was fine and the next auto-update would install a browser-less bundle.
#
# Usage: stage-cef.sh [identifier]. Requires the helper binary and the CEF
# distribution, both produced by a release build; this runs in a bundle
# recipe before `tauri build`.
set -euo pipefail

if [ "$(uname -s)" != "Darwin" ]; then
    exit 0
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
IDENTIFIER="${1:-dev.vedhavyas.forge}"
HELPER_BIN="$TARGET/release/forge_client_helper"
CEF_DIR="$(find "$TARGET/release/build/cef-dll-sys" -type d -path '*/out/cef_macos_aarch64' 2>/dev/null | head -1)"

if [ -z "$CEF_DIR" ] || [ ! -d "$CEF_DIR/Chromium Embedded Framework.framework" ]; then
    echo "[ERROR] the CEF distribution is not unpacked under $TARGET/release/build - build the client first" >&2
    exit 1
fi
if [ ! -x "$HELPER_BIN" ]; then
    echo "[ERROR] no helper binary at $HELPER_BIN - build forge_client_helper first" >&2
    exit 1
fi

# The executable name the tauri bundle names in Contents/MacOS.
MAIN=forge-client
VERSION=$(grep -m1 '^version = ' "$ROOT/Cargo.toml" | cut -d'"' -f2)
STAGE="$ROOT/cef-bundle"
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp -R "$CEF_DIR/Chromium Embedded Framework.framework" "$STAGE/"

write_plist() {
    local name="$1" app="$2"
    cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>$name</string>
	<key>CFBundleIdentifier</key>
	<string>$IDENTIFIER</string>
	<key>CFBundleDisplayName</key>
	<string>$name</string>
	<key>CFBundleDevelopmentRegion</key>
	<string>English</string>
	<key>CFBundleVersion</key>
	<string>$VERSION</string>
	<key>CFBundleExecutable</key>
	<string>$name</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleSignature</key>
	<string>????</string>
	<key>CFBundleShortVersionString</key>
	<string>$VERSION</string>
	<key>LSFileQuarantineEnabled</key>
	<true/>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>LSUIElement</key>
	<string>1</string>
	<key>NSSupportsAutomaticGraphicsSwitching</key>
	<true/>
</dict>
</plist>
PLIST
}

for SUFFIX in "Helper (GPU)" "Helper (Renderer)" "Helper (Plugin)" "Helper (Alerts)" "Helper"; do
    NAME="$MAIN $SUFFIX"
    APP="$STAGE/$NAME.app"
    mkdir -p "$APP/Contents/MacOS"
    cp "$HELPER_BIN" "$APP/Contents/MacOS/$NAME"
    write_plist "$NAME" "$APP"
    codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || {
        echo "[ERROR] the helper app for '$SUFFIX' could not be signed" >&2
        exit 3
    }
done

echo "[OK] CEF staged in $STAGE (framework + 5 helpers, version $VERSION)"
