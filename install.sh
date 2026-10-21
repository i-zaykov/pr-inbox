#!/bin/zsh
# Builds PR Inbox and installs it into ~/Applications/PR Inbox.app (Raycast and Spotlight find it there),
# together with the login agent that runs its server. Works on a fresh Mac and over an earlier install.
set -euo pipefail
cd "$(dirname "$0")"
NAME="PR Inbox"
LABEL="io.github.i-zaykov.pr-inbox"
# The agent and the window get the same port, so a custom port never splits the app into two servers.
PORT="${PR_INBOX_PORT:-4477}"
# Where your clones live, one folder per repo named like it (~/code/my-repo); both processes get it too.
CHECKOUTS="${PR_INBOX_CHECKOUTS:-$HOME/code}"
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
APP="$HOME/Applications/$NAME.app"
BIN="$APP/Contents/MacOS/pr-inbox"
AGENT="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG="$HOME/Library/Logs/$NAME.log"
echo "1/3 Building (release)"
cargo build --release
echo "2/3 Installing into $APP"
# Stop the login agent and the window first: macOS kills a process whose signed binary changes under it.
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
# An agent installed under an earlier label would keep restarting its own server on the same port.
for old in "$HOME"/Library/LaunchAgents/*.pr-inbox.plist(N); do
  [[ "$old" != "$AGENT" ]] && grep -q "$BIN" "$old" || continue
  launchctl bootout "gui/$(id -u)" "$old" 2>/dev/null || true
  /bin/rm -f "$old"
done
# Anchored to the start of the command line: a running Claude review has this path in its prompt, and must
# outlive the restart (the new server adopts it).
pkill -f "^${BIN//./\\.}( |\$)" 2>/dev/null || true
sleep 1
/bin/mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
/bin/cp -f target/release/pr-inbox "$BIN"
/bin/cp -f icons/icon.icns "$APP/Contents/Resources/AppIcon.icns"
# Info.plist goes in before codesign, which seals it. LSEnvironment hands the port to the window.
cat >| "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDisplayName</key>
	<string>$NAME</string>
	<key>CFBundleExecutable</key>
	<string>pr-inbox</string>
	<key>CFBundleIconFile</key>
	<string>AppIcon</string>
	<key>CFBundleIdentifier</key>
	<string>$LABEL</string>
	<key>CFBundleName</key>
	<string>$NAME</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$VERSION</string>
	<key>CFBundleVersion</key>
	<string>$VERSION</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.developer-tools</string>
	<key>LSEnvironment</key>
	<dict>
		<key>PR_INBOX_PORT</key>
		<string>$PORT</string>
		<key>PR_INBOX_CHECKOUTS</key>
		<string>$CHECKOUTS</string>
	</dict>
	<key>LSMinimumSystemVersion</key>
	<string>13.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
EOF
plutil -lint "$APP/Contents/Info.plist"
codesign --force --deep --sign - "$APP"
# Launch Services caches Info.plist; re-register so it picks up the icon, version and LSEnvironment.
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$APP"
echo "3/3 Starting the background server and opening the window"
/bin/mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
cat >| "$AGENT" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$BIN</string>
    <string>--server</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PR_INBOX_PORT</key><string>$PORT</string>
    <key>PR_INBOX_CHECKOUTS</key><string>$CHECKOUTS</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>StandardOutPath</key><string>$LOG</string>
  <key>StandardErrorPath</key><string>$LOG</string>
</dict>
</plist>
EOF
plutil -lint "$AGENT"
launchctl bootstrap "gui/$(id -u)" "$AGENT"
# A freshly signed binary can take a few seconds to start; open the window only once the server listens,
# or the window starts its own server first and the agent keeps exiting on the taken port.
up=0
for _ in {1..30}; do curl -fs -o /dev/null --max-time 1 "http://localhost:$PORT/" && { up=1; break; }; sleep 0.5; done
if (( ! up )); then
  echo "The server did not answer on port $PORT. The last lines of $LOG:"
  tail -20 "$LOG" || true
fi
open "$APP"
