#!/bin/zsh
set -euo pipefail
cd "$(dirname "$0")"
APP="$HOME/Applications/PR Inbox Link Router.app"
BIN="$APP/Contents/MacOS/pr-inbox-link-router"
BUILD=$(mktemp -d)
trap 'rm -rf "$BUILD"' EXIT
swiftc -O main.swift -o "$BUILD/pr-inbox-link-router"
"$BUILD/pr-inbox-link-router" --self-test
# Compile first, then replace the installed executable without editing a running signed binary.
pkill -f "^${BIN//./\\.}( |\$)" 2>/dev/null || true
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BUILD/pr-inbox-link-router" "$BIN"
cp ../../icons/icon.icns "$APP/Contents/Resources/AppIcon.icns"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>io.github.i-zaykov.pr-inbox.link-router</string>
  <key>CFBundleName</key><string>PR Inbox Link Router</string>
  <key>CFBundleDisplayName</key><string>PR Inbox Link Router</string>
  <key>CFBundleExecutable</key><string>pr-inbox-link-router</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>CFBundleShortVersionString</key><string>1.0</string>
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>CFBundleURLTypes</key><array><dict>
    <key>CFBundleTypeRole</key><string>Viewer</string>
    <key>LSHandlerRank</key><string>Alternate</string>
    <key>CFBundleURLSchemes</key><array><string>http</string><string>https</string></array>
  </dict></array>
</dict></plist>
PLIST
plutil -lint "$APP/Contents/Info.plist"
codesign --force --deep --sign - "$APP"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$APP"
if [[ "${1:-}" == "--enable" ]]; then
  "$BIN" --enable
else
  echo "Installed. Enable with: '$BIN' --enable"
fi
