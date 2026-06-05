#!/usr/bin/env bash
#
# Package a macOS .app bundle (ad-hoc signed) and zip it for distribution.
#
# clip-llm runs with an Accessory activation policy (no Dock icon) and needs
# Accessibility permission to simulate Cmd+C/Cmd+V. A bare CLI binary cannot get
# its own TCC grant (it is attributed to the launching terminal), so macOS must
# ship as a proper .app bundle launched via LaunchServices.
#
# Usage: scripts/package-macos-app.sh <binary-path> <version> <output-zip>
#
set -euo pipefail

BIN="${1:?binary path required}"
VERSION="${2:?version required}"
OUT="${3:?output zip path required}"

APP="clip-llm.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$BIN" "$APP/Contents/MacOS/clip-llm"
chmod +x "$APP/Contents/MacOS/clip-llm"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>clip-llm</string>
  <key>CFBundleDisplayName</key><string>clip-llm</string>
  <key>CFBundleIdentifier</key><string>com.zekaizer.clip-llm</string>
  <key>CFBundleExecutable</key><string>clip-llm</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
</dict>
</plist>
PLIST

# Ad-hoc sign so TCC tracks a stable identity per release build.
codesign --force --deep --sign - "$APP"
codesign --verify --verbose "$APP"

# .app is a directory; ditto preserves the bundle structure inside a zip.
rm -f "$OUT"
ditto -c -k --keepParent "$APP" "$OUT"

echo "packaged $OUT (clip-llm $VERSION)"
