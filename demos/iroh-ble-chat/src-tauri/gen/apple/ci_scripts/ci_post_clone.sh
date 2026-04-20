#!/bin/bash
set -euo pipefail

# Xcode Cloud ci_post_clone script
# Installs mise + toolchain, builds the frontend, and cross-compiles
# the Rust static library (libapp.a) so that Xcode Cloud's native
# xcodebuild archive step only needs to link it.

# Retry wrapper for network-dependent commands. Xcode Cloud builders
# regularly hit transient DNS failures; retry with exponential backoff.
retry() {
  local max_attempts=6
  local attempt=1
  local delay=5
  while true; do
    if "$@"; then
      return 0
    fi
    if [ "$attempt" -ge "$max_attempts" ]; then
      echo "==> Command failed after $max_attempts attempts: $*" >&2
      return 1
    fi
    echo "==> Attempt $attempt failed, retrying in ${delay}s: $*" >&2
    sleep "$delay"
    attempt=$((attempt + 1))
    delay=$((delay * 2))
  done
}

echo "==> Waiting for network/DNS readiness"
for i in $(seq 1 30); do
  if curl -fsS --max-time 10 https://static.rust-lang.org/dist/channel-rust-stable.toml >/dev/null; then
    echo "==> Network ready after $i attempt(s)"
    break
  fi
  if [ "$i" -eq 30 ]; then
    echo "==> Network never became ready after 30 attempts" >&2
    exit 1
  fi
  sleep 5
done

echo "==> Installing mise"
retry curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 --max-time 120 https://mise.run -o /tmp/mise-installer.sh
sh /tmp/mise-installer.sh
export PATH="$HOME/.local/bin:$PATH"

echo "==> Installing toolchain via mise"
export MISE_COLOR=false
export MISE_HTTP_TIMEOUT=120
retry mise install
export PATH="$HOME/.local/share/mise/shims:$HOME/.cargo/bin:$PATH"

echo "==> Adding iOS target"
retry rustup target add aarch64-apple-ios

CHAT_DIR="$CI_PRIMARY_REPOSITORY_PATH/demos/iroh-ble-chat"

echo "==> Installing frontend dependencies"
cd "$CHAT_DIR"
retry pnpm install --frozen-lockfile

echo "==> Building frontend"
pnpm build

echo "==> Copying frontend dist into Xcode assets"
cp -r "$CHAT_DIR/dist" "$CHAT_DIR/src-tauri/gen/apple/assets"

echo "==> Fetching cargo dependencies"
cd "$CHAT_DIR/src-tauri"
export IPHONEOS_DEPLOYMENT_TARGET=16.0
retry cargo fetch --target aarch64-apple-ios

echo "==> Building Rust static library for iOS"
cargo build --release --target aarch64-apple-ios --lib --features tauri/custom-protocol --offline

echo "==> Copying libapp.a into Externals"
EXTERNALS="$CI_PRIMARY_REPOSITORY_PATH/demos/iroh-ble-chat/src-tauri/gen/apple/Externals/arm64/release"
mkdir -p "$EXTERNALS"
cp "$CI_PRIMARY_REPOSITORY_PATH/target/aarch64-apple-ios/release/libiroh_ble_chat_lib.a" "$EXTERNALS/libapp.a"

echo "==> Done. Toolchain versions:"
rustc --version
node --version
pnpm --version
ls -lh "$EXTERNALS/libapp.a"
