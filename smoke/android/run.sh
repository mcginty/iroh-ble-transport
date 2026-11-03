#!/usr/bin/env bash
# Build the smoke app and/or launch it on the connected device or emulator.
# The launch passes only if the app logs IROH_BLE_SMOKE_OK and is still running a
# few seconds later.
#
#   smoke/android/run.sh [build|launch|all] [aarch64|x86_64]
#
# Defaults: all, x86_64. IROH_BLE_SMOKE_LATEST=1 runs `cargo update` before the
# build, so it uses the newest tauri/tao/wry the version requirements allow
# instead of the lockfile.
set -euo pipefail

cd "$(dirname "$0")"
step=${1:-all}
target=${2:-x86_64}
package=org.jakebot.irohble.smoke
timeout_secs=${IROH_BLE_SMOKE_TIMEOUT:-90}

build() {
    if [[ -z "${NDK_HOME:-}" ]]; then
        NDK_HOME=$(find "${ANDROID_HOME:?ANDROID_HOME must be set}/ndk" -mindepth 1 -maxdepth 1 -type d | sort -V | tail -1)
        export NDK_HOME
    fi
    if [[ "${IROH_BLE_SMOKE_LATEST:-0}" == 1 ]]; then
        cargo update
    fi
    cargo tree -i tao --depth 0 --target all
    cargo tree -i blew --depth 0 --target all
    cargo tauri android build --debug --apk --target "$target"
}

# Capture before matching. Under pipefail, `adb logcat | grep -q` reports a
# match as a failure once the log is long enough: grep exits on the match and
# adb dies of SIGPIPE writing the rest.
logged() {
    local log
    log=$(adb logcat -d -s iroh-ble-smoke:V)
    grep -qE "$1" <<<"$log"
}

# The crash buffer truncates the process name ("t.irohble.smoke"), and a Java
# FATAL EXCEPTION names the full package, so match the common tail.
crashed() {
    local log
    log=$(adb logcat -d -b crash)
    grep -q 'irohble\.smoke' <<<"$log"
}

# A panic on a Tokio worker kills only its task, so the process can outlive
# one; the marker is the only sign of it.
failed() {
    logged 'IROH_BLE_SMOKE_(FAIL|PANIC)'
}

launch() {
    local apk
    apk=$(find gen/android/app/build/outputs/apk -name '*.apk' | head -1)
    [[ -n "$apk" ]] || { echo "no APK found; run the build step first" >&2; exit 1; }

    adb wait-for-device
    adb uninstall "$package" >/dev/null 2>&1 || true
    # -g grants the BLE runtime permissions, so no dialog is needed.
    adb install -g "$apk"
    adb logcat -c
    adb logcat -b crash -c || true
    adb shell am start -W -n "$package/.MainActivity"

    local verdict="" deadline=$((SECONDS + timeout_secs))
    while ((SECONDS < deadline)); do
        if failed; then
            verdict=fail
            break
        fi
        if crashed; then
            verdict=crash
            break
        fi
        if logged IROH_BLE_SMOKE_OK; then
            verdict=ok
            break
        fi
        sleep 2
    done

    # A failure shortly after the verdict still counts.
    if [[ "$verdict" == ok ]]; then
        sleep 5
        if failed; then
            verdict=fail
        elif crashed || [[ -z "$(adb shell pidof "$package" || true)" ]]; then
            verdict=crash
        fi
    fi

    echo "--- app log ---"
    adb logcat -d -s iroh-ble-smoke:V RustStdoutStderr:V || true
    echo "--- crash buffer ---"
    adb logcat -d -b crash || true

    case "$verdict" in
        ok) echo "smoke test passed" ;;
        "") echo "smoke test timed out after ${timeout_secs}s with no verdict" >&2; exit 1 ;;
        *) echo "smoke test failed: $verdict" >&2; exit 1 ;;
    esac
}

case "$step" in
    build) build ;;
    launch) launch ;;
    all) build; launch ;;
    *) echo "usage: $0 [build|launch|all] [aarch64|x86_64]" >&2; exit 2 ;;
esac
