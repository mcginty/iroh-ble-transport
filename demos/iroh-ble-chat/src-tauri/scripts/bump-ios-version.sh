#!/bin/bash
set -euo pipefail

# cargo-release pre-release-hook. Runs with cwd = src-tauri (CRATE_ROOT).
# Syncs the iOS bundle version with the Cargo.toml version being released.
#
# - CFBundleShortVersionString := Cargo version with any prerelease suffix
#   stripped (Apple requires X.Y.Z integers, so "0.1.0-alpha.4" -> "0.1.0").
# - CFBundleVersion := monotonic integer from git history, so every alpha
#   upload has a strictly-increasing build number even when the short
#   version is unchanged.

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
