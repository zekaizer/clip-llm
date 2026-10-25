#!/usr/bin/env bash
#
# Package a macOS .app bundle (ad-hoc signed) and zip it for distribution.
#
# clip-llm runs with an Accessory activation policy (no Dock icon) and needs
# Accessibility permission to simulate Cmd+C/Cmd+V. A bare CLI binary cannot get
# its own TCC grant (it is attributed to the launching terminal), so macOS must
# ship as a proper .app bundle launched via LaunchServices.
#
# Usage: scripts/package-macos-app.sh <binary-path> <version> <output-zip> [--config <config.toml>]
#
# --config copies an existing config.toml into the bundle on disk after the
# zip is made: the zip is the distributable and must not carry API keys. The
# file lands outside the code signature, where the app itself writes it.
#
# Signs ad hoc unless CODESIGN_IDENTITY (and CODESIGN_KEYCHAIN) are set; a
# local build takes them from scripts/macos-dev-signing.sh so the
# Accessibility grant survives rebuilds.
#
set -euo pipefail

BIN="${1:?binary path required}"
VERSION="${2:?version required}"
OUT="${3:?output zip path required}"
CONFIG=""
if [ $# -gt 3 ]; then
  [ "$4" = "--config" ] && [ -n "${5:-}" ] || { echo "usage: $0 <binary> <version> <zip> [--config <config.toml>]" >&2; exit 2; }
  [ -f "$5" ] || { echo "config not found: $5" >&2; exit 1; }
  # Copied now: the source may sit inside the bundle that is rebuilt below.
  CONFIG="$(mktemp)"
  trap 'rm -f "$CONFIG"' EXIT
  cp "$5" "$CONFIG"
  CONFIG_SRC="$5"
fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

APP="clip-llm.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/clip-llm"
chmod +x "$APP/Contents/MacOS/clip-llm"
cp "$REPO_ROOT/assets/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>clip-llm</string>
  <key>CFBundleDisplayName</key><string>clip-llm</string>
  <key>CFBundleIdentifier</key><string>com.zekaizer.clip-llm</string>
  <key>CFBundleExecutable</key><string>clip-llm</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSLocalNetworkUsageDescription</key><string>clip-llm ships diagnostic logs to a VictoriaLogs server on your local network when [logging].url is configured.</string>
</dict>
</plist>
PLIST

sign_args=(--force --deep --sign "${CODESIGN_IDENTITY:--}")
[ -n "${CODESIGN_KEYCHAIN:-}" ] && sign_args+=(--keychain "$CODESIGN_KEYCHAIN")
codesign "${sign_args[@]}" "$APP"
codesign --verify --verbose "$APP"

# .app is a directory; ditto preserves the bundle structure inside a zip.
rm -f "$OUT"
ditto -c -k --keepParent "$APP" "$OUT"

echo "packaged $OUT (clip-llm $VERSION)"

if [ -n "$CONFIG" ]; then
  install -m 600 "$CONFIG" "$APP/Contents/MacOS/config.toml"
  echo "bundled config from $CONFIG_SRC into $APP (not in $OUT)"
fi
