#!/bin/bash
# Assemble OmniPTY.app from the release binary.
#
#   scripts/bundle.sh              build + bundle + ad-hoc sign
#   SIGN_ID="Developer ID Application: ..." scripts/bundle.sh   real signature
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"
APP="$ROOT/target/OmniPTY.app"
BIN="$ROOT/target/release/omnipty"
SIGN_ID="${SIGN_ID:--}"

cargo build --release

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/omnipty"

# Icon: source artwork -> 1024 master -> .iconset -> .icns
if [[ ! -f "$ROOT/assets/icon_1024.png" ]]; then
  python3 "$ROOT/assets/prepare_icon.py"
fi
ICONSET="$ROOT/target/omnipty.iconset"
rm -rf "$ICONSET"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z $size $size "$ROOT/assets/icon_1024.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  sips -z $((size * 2)) $((size * 2)) "$ROOT/assets/icon_1024.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/omnipty.icns"

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
cat >"$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>dev.bobbycoleman.omnipty</string>
	<key>CFBundleName</key>
	<string>OmniPTY</string>
	<key>CFBundleDisplayName</key>
	<string>OmniPTY</string>
	<key>CFBundleExecutable</key>
	<string>omnipty</string>
	<key>CFBundleIconFile</key>
	<string>omnipty</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>${VERSION}</string>
	<key>CFBundleVersion</key>
	<string>${VERSION}</string>
	<key>LSMinimumSystemVersion</key>
	<string>12.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<!-- Without these, macOS shows an unattributed prompt for the protected
	     folders and the grant is not reliably recorded against the app. -->
	<key>NSDesktopFolderUsageDescription</key>
	<string>OmniPTY needs access to show your Desktop in the file tree and to run commands there.</string>
	<key>NSDocumentsFolderUsageDescription</key>
	<string>OmniPTY needs access to show your Documents in the file tree and to run commands there.</string>
	<key>NSDownloadsFolderUsageDescription</key>
	<string>OmniPTY needs access to show your Downloads in the file tree and to run commands there.</string>
	<key>NSRemovableVolumesUsageDescription</key>
	<string>OmniPTY needs access to removable volumes to browse and run commands on them.</string>
	<key>NSNetworkVolumesUsageDescription</key>
	<string>OmniPTY needs access to network volumes to browse and run commands on them.</string>
  <!-- Programs run inside OmniPTY are charged to OmniPTY by macOS; without these their requests are denied silently (#11). -->
	<key>NSCalendarsUsageDescription</key>
	<string>A program running in OmniPTY wants to access your calendars.</string>
	<key>NSCalendarsFullAccessUsageDescription</key>
	<string>A program running in OmniPTY wants to access your calendars.</string>
	<key>NSRemindersUsageDescription</key>
	<string>A program running in OmniPTY wants to access your reminders.</string>
	<key>NSRemindersFullAccessUsageDescription</key>
	<string>A program running in OmniPTY wants to access your reminders.</string>
	<key>NSContactsUsageDescription</key>
	<string>A program running in OmniPTY wants to access your contacts.</string>
	<key>NSPhotoLibraryUsageDescription</key>
	<string>A program running in OmniPTY wants to access your photo library.</string>
	<key>NSAppleEventsUsageDescription</key>
	<string>A program running in OmniPTY wants to control another app.</string>
	<key>NSCameraUsageDescription</key>
	<string>A program running in OmniPTY wants to use the camera.</string>
	<key>NSMicrophoneUsageDescription</key>
	<string>A program running in OmniPTY wants to use the microphone.</string>
	<key>NSSupportsAutomaticGraphicsSwitching</key>
	<true/>
</dict>
</plist>
PLIST

ENTITLEMENTS="$ROOT/scripts/OmniPTY.entitlements"

# Terminal emulators must not be sandboxed; ad-hoc signing ("-") is enough to
# quiet Gatekeeper on your own machine. Hardened runtime only with a real ID.
# entitlements are hardened-runtime exceptions, not sandbox ones
if [[ "$SIGN_ID" == "-" ]]; then
  codesign --force --sign - "$APP"
else
  codesign --deep --force --options runtime \
    --entitlements "$ENTITLEMENTS" --sign "$SIGN_ID" "$APP"
fi

echo "built $APP"
