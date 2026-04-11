#!/usr/bin/env bash
set -e

COUNT=0
DEVICES=$(xcrun devicectl list devices 2>/dev/null | awk '/available/ && !/unavailable|simulator|Simulator/ {for(i=1;i<=NF;i++) if($i ~ /^[0-9A-Fa-f-]{20,}$/) print $i}')

if [ -z "$DEVICES" ]; then
  echo "No iOS devices connected."
  exit 0
fi

echo "==> Building frontend..."
pnpm build
echo "==> Building iOS debug app..."
CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_DEV_STRIP=debuginfo \
  TAURI_CLI_NO_DEV_SERVER=true pnpm tauri ios build --debug

APP_PATH=$(find src-tauri/gen/apple/build -name "*.app" -not -path "*/Simulator/*" 2>/dev/null | head -1)
if [ -z "$APP_PATH" ]; then
  echo "ERROR: Could not find built .app bundle"
  exit 1
fi

for udid in $DEVICES; do
  echo "==> Installing on $udid..."
  xcrun devicectl device install app --device "$udid" "$APP_PATH" 2>&1 || { echo "    WARNING: Failed to install on $udid"; continue; }
  echo "==> Launching on $udid..."
  xcrun devicectl device process launch --device "$udid" org.jakebot.iroh-ble-chat 2>&1 || echo "    WARNING: Failed to launch on $udid"
  COUNT=$((COUNT + 1))
done

echo "==> Deployed to $COUNT iOS device(s)."
