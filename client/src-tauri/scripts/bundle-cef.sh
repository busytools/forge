#!/usr/bin/env bash
# Put CEF inside a built forge.app: the framework, and the five helper apps
# CEF re-enters for its subprocesses.
#
# **Layout only.** The framework is dlopen'd, never linked, so there is no
# rpath work; CEF's own loader finds everything by position - the framework
# at Contents/Frameworks, the helpers named after the main executable.
#
# Usage: bundle-cef.sh <forge.app> <cef dir> <helper binary> [identifier]
set -euo pipefail
APP="${1:?the built app bundle}"
CEF_DIR="${2:?the CEF distribution directory}"
HELPER_BIN="${3:?the built forge_client_helper binary}"
IDENTIFIER="${4:-dev.vedhavyas.forge}"

FRAMEWORKS="$APP/Contents/Frameworks"
MAIN=$(/usr/libexec/PlistBuddy -c 'Print CFBundleExecutable' "$APP/Contents/Info.plist")
VERSION=$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$APP/Contents/Info.plist")

if [ ! -d "$CEF_DIR/Chromium Embedded Framework.framework" ]; then
    echo "no CEF framework under $CEF_DIR" >&2
    exit 2
fi
if [ ! -x "$HELPER_BIN" ]; then
    echo "no helper binary at $HELPER_BIN" >&2
    exit 2
fi

mkdir -p "$FRAMEWORKS"
rm -rf "$FRAMEWORKS/Chromium Embedded Framework.framework"
cp -R "$CEF_DIR/Chromium Embedded Framework.framework" "$FRAMEWORKS/"

write_plist() {
    local name="$1" app="$2"
    cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>$MAIN</string>
	<key>CFBundleIdentifier</key>
	<string>$IDENTIFIER</string>
	<key>CFBundleDisplayName</key>
	<string>$MAIN</string>
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
	<key>LSEnvironment</key>
	<dict>
		<key>MallocNanoZone</key>
		<string>0</string>
	</dict>
	<key>LSFileQuarantineEnabled</key>
	<true/>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>LSUIElement</key>
	<string>1</string>
	<key>NSBluetoothAlwaysUsageDescription</key>
	<string>$name</string>
	<key>NSSupportsAutomaticGraphicsSwitching</key>
	<true/>
	<key>NSWebBrowserPublicKeyCredentialUsageDescription</key>
	<string>$name</string>
	<key>NSCameraUsageDescription</key>
	<string>$name</string>
	<key>NSMicrophoneUsageDescription</key>
	<string>$name</string>
</dict>
</plist>
PLIST
}

for SUFFIX in "Helper (GPU)" "Helper (Renderer)" "Helper (Plugin)" "Helper (Alerts)" "Helper"; do
    NAME="$MAIN $SUFFIX"
    HELPER_APP="$FRAMEWORKS/$NAME.app"
    rm -rf "$HELPER_APP"
    mkdir -p "$HELPER_APP/Contents/MacOS"
    cp "$HELPER_BIN" "$HELPER_APP/Contents/MacOS/$NAME"
    write_plist "$NAME" "$HELPER_APP"
done

echo "[OK] CEF is in $APP (framework + 5 helpers, main executable $MAIN $VERSION)"
