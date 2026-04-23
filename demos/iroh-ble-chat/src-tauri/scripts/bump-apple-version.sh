#!/bin/bash
set -euo pipefail

# cargo-release pre-release-hook. Runs with cwd = src-tauri (CRATE_ROOT).
# Patches tauri.conf.json and the checked-in Apple project metadata so
# cargo-release, Xcode Cloud, and the GitHub Actions release pipeline
# agree on the same version/build number.
#
# - version := Cargo version with prerelease suffix stripped
#   (Apple requires X.Y.Z integers, so "0.1.0-alpha.4" -> "0.1.0").
# - bundle.iOS.bundleVersion   := monotonic integer from git history.
# - bundle.macOS.bundleVersion := same integer (TestFlight requires a
#   strictly-increasing CFBundleVersion per CFBundleShortVersionString).
# - Android versionCode is handled by Tauri's autoIncrementVersionCode.

: "${NEW_VERSION:?NEW_VERSION not set — this script must be run by cargo-release}"

SHORT_VERSION="${NEW_VERSION%%-*}"
BUILD_NUMBER="$(git rev-list --count HEAD)"

echo -en "\033[1;32m     Bumping\033[0m Apple release metadata (version = $SHORT_VERSION, build = $BUILD_NUMBER)"
if [[ "$DRY_RUN" == "true" ]]; then
  echo -e "\033[1;33m [dry run, no files touched]\033[0m"
  exit
fi

echo ""

JQ_BIN="${JQ_BIN:-$(command -v jq 2>/dev/null || true)}"
YQ_BIN="${YQ_BIN:-$(command -v yq 2>/dev/null || true)}"
PLIST_BUDDY="${PLIST_BUDDY:-/usr/libexec/PlistBuddy}"

[[ -n "$JQ_BIN" ]] || { echo "jq not found" >&2; exit 1; }
if [[ -z "$YQ_BIN" ]] && command -v mise >/dev/null 2>&1; then
  YQ_BIN="$(mise which yq 2>/dev/null || true)"
fi
[[ -n "$YQ_BIN" ]] || { echo "yq not found" >&2; exit 1; }
[[ -x "$PLIST_BUDDY" ]] || { echo "PlistBuddy not found at $PLIST_BUDDY" >&2; exit 1; }

tmp_json="$(mktemp "${TMPDIR:-/tmp}/tauri.conf.json.XXXXXX")"
trap 'rm -f "$tmp_json"' EXIT

"$JQ_BIN" --indent 2 \
  --arg version "$SHORT_VERSION" \
  --arg build "$BUILD_NUMBER" \
  '
    .version = $version
    | .bundle.iOS.bundleVersion = $build
    | .bundle.macOS.bundleVersion = $build
  ' \
  tauri.conf.json > "$tmp_json"
mv "$tmp_json" tauri.conf.json
trap - EXIT

SHORT_VERSION="$SHORT_VERSION" BUILD_NUMBER="$BUILD_NUMBER" "$YQ_BIN" -i '
  .targets."iroh-ble-chat_iOS".info.properties.CFBundleShortVersionString = strenv(SHORT_VERSION)
  | .targets."iroh-ble-chat_iOS".info.properties.CFBundleVersion = strenv(BUILD_NUMBER)
' gen/apple/project.yml

"$PLIST_BUDDY" \
  -c "Set :CFBundleShortVersionString $SHORT_VERSION" \
  -c "Set :CFBundleVersion $BUILD_NUMBER" \
  gen/apple/iroh-ble-chat_iOS/Info.plist
