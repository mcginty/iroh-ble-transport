#!/bin/bash
set -euo pipefail

# cargo-release pre-release-hook. Runs with cwd = src-tauri (CRATE_ROOT).
# Patches tauri.conf.json so Tauri writes the correct values into Info.plist
# and build.gradle at build time.
#
# - version := Cargo version with prerelease suffix stripped
#   (Apple requires X.Y.Z integers, so "0.1.0-alpha.4" -> "0.1.0").
# - bundle.iOS.bundleVersion := monotonic integer from git history, so
#   every alpha upload has a strictly-increasing build number.
# - Android versionCode is handled by Tauri's autoIncrementVersionCode.

: "${NEW_VERSION:?NEW_VERSION not set — this script must be run by cargo-release}"

SHORT_VERSION="${NEW_VERSION%%-*}"
BUILD_NUMBER="$(git rev-list --count HEAD)"

echo -en "\033[1;32m     Bumping\033[0m tauri.conf.json (version = $SHORT_VERSION, bundle.iOS.bundleVersion = $BUILD_NUMBER)"
if [[ "$DRY_RUN" == "true" ]]; then
  echo -e "\033[1;33m [dry run, no files touched]\033[0m"
  exit
fi

echo ""

python3 -c "
import json, sys
p, ver, build = sys.argv[1], sys.argv[2], sys.argv[3]
with open(p) as f:
    d = json.load(f)
d['version'] = ver
d.setdefault('bundle', {}).setdefault('iOS', {})['bundleVersion'] = build
with open(p, 'w') as f:
    json.dump(d, f, indent=2)
    f.write('\n')
" tauri.conf.json "$SHORT_VERSION" "$BUILD_NUMBER"
