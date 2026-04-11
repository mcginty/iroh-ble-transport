#!/usr/bin/env bash
set -e

COUNT=0
DEVICES=$(adb devices 2>/dev/null | grep -w 'device$' | awk '{print $1}')
if [ -z "$DEVICES" ]; then
  echo "No Android devices connected."
  exit 0
fi

echo "==> Building frontend..."
pnpm build
echo "==> Building Android debug APK..."
CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_DEV_STRIP=debuginfo \
  TAURI_CLI_NO_DEV_SERVER=true pnpm tauri android build --debug --apk --target aarch64
APK="src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk"

for dev in $DEVICES; do
  echo "==> Installing on $dev..."
  adb -s "$dev" install -r "$APK"
  echo "==> Launching on $dev..."
  adb -s "$dev" shell am start -n org.jakebot.iroh_ble_chat/.MainActivity
  COUNT=$((COUNT + 1))
done

echo "==> Deployed to $COUNT Android device(s)."
