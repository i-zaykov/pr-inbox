#!/bin/zsh
# T3's daily 08:00 task calls this. Keep the existing filename for installed integrations.
set -euo pipefail
hour=$(date +%H)
if (( 10#$hour < 8 || 10#$hour >= 17 )); then
  echo "Skipped the missed morning start. Scheduled batches start between 08:00 and 17:00."
  exit 0
fi
label=io.github.i-zaykov.pr-inbox
agent="$HOME/Library/LaunchAgents/$label.plist"
port=$(/usr/libexec/PlistBuddy -c 'Print :EnvironmentVariables:PR_INBOX_PORT' "$agent" 2>/dev/null || true)
port=${port:-4477}
if ! curl -fsS --max-time 5 -H 'x-pr-inbox: 1' "http://localhost:$port/api/overnight" >/dev/null; then
  launchctl kickstart "gui/$(id -u)/$label"
fi
# Starting the same day twice returns the existing batch, including after a stopped/completed pass.
curl -fsS --max-time 30 --retry 3 --retry-connrefused --retry-delay 1 \
  -X POST -H 'x-pr-inbox: 1' "http://localhost:$port/api/overnight/start"
echo
