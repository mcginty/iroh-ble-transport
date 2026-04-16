#!/bin/bash
set -euo pipefail

# cargo-release pre-release-hook. Runs with cwd = src-tauri (CRATE_ROOT).
# Syncs iOS bundle version and tauri.conf.json with the Cargo.toml version
# being released.
#
# - CFBundleShortVersionString / tauri.conf.json version := Cargo version
#   with any prerelease suffix stripped (Apple requires X.Y.Z integers,
#   so "0.1.0-alpha.4" -> "0.1.0").
# - CFBundleVersion := monotonic integer from git history, so every alpha
#   upload has a strictly-increasing build number even when the short
#   version is unchanged.
# - Android versionCode is handled by Tauri's autoIncrementVersionCode.

: "${NEW_VERSION:?NEW_VERSION not set — this script must be run by cargo-release}"

SHORT_VERSION="${NEW_VERSION%%-*}"
BUILD_NUMBER="$(git rev-list --count HEAD)"

PLIST="gen/apple/iroh-ble-chat_iOS/Info.plist"
PROJECT_YML="gen/apple/project.yml"

echo "==> Bumping iOS bundle version"
echo "    CFBundleShortVersionString = $SHORT_VERSION"
echo "    CFBundleVersion            = $BUILD_NUMBER"

plutil -replace CFBundleShortVersionString -string "$SHORT_VERSION" "$PLIST"
plutil -replace CFBundleVersion -string "$BUILD_NUMBER" "$PLIST"

sed -i '' "s/^\([[:space:]]*CFBundleShortVersionString:\).*/\1 $SHORT_VERSION/" "$PROJECT_YML"
sed -i '' "s/^\([[:space:]]*CFBundleVersion:\).*/\1 \"$BUILD_NUMBER\"/" "$PROJECT_YML"

# --- tauri.conf.json (feeds iOS versionName via Tauri) ---

TAURI_CONF="tauri.conf.json"

echo "==> Bumping tauri.conf.json version"
echo "    version = $SHORT_VERSION"

python3 -c "
import json, sys
p = sys.argv[1]
with open(p) as f:
    d = json.load(f)
d['version'] = sys.argv[2]
with open(p, 'w') as f:
    json.dump(d, f, indent=2)
    f.write('\n')
" "$TAURI_CONF" "$SHORT_VERSION"
