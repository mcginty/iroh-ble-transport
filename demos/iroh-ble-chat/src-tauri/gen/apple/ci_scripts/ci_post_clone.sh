#!/bin/bash
set -euo pipefail

# Xcode Cloud ci_post_clone script
# Installs mise + toolchain, builds the frontend, and cross-compiles
# the Rust static library (libapp.a) so that Xcode Cloud's native
# xcodebuild archive step only needs to link it.

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
curl https://mise.run | sh
export PATH="$HOME/.local/bin:$PATH"

echo "==> Installing toolchain via mise"
export MISE_COLOR=false
export MISE_HTTP_TIMEOUT=120
mise install
export PATH="$HOME/.local/share/mise/shims:$HOME/.cargo/bin:$PATH"

echo "==> Adding iOS target"
rustup target add aarch64-apple-ios

CHAT_DIR="$CI_PRIMARY_REPOSITORY_PATH/demos/iroh-ble-chat"

echo "==> Installing frontend dependencies"
cd "$CHAT_DIR"
pnpm install --frozen-lockfile

echo "==> Building frontend"
pnpm build

echo "==> Copying frontend dist into Xcode assets"
cp -r "$CHAT_DIR/dist" "$CHAT_DIR/src-tauri/gen/apple/assets"

echo "==> Building Rust static library for iOS"
cd "$CHAT_DIR/src-tauri"
export IPHONEOS_DEPLOYMENT_TARGET=16.0
cargo build --release --target aarch64-apple-ios --lib

echo "==> Copying libapp.a into Externals"
EXTERNALS="$CI_PRIMARY_REPOSITORY_PATH/demos/iroh-ble-chat/src-tauri/gen/apple/Externals/arm64/release"
mkdir -p "$EXTERNALS"
cp "$CI_PRIMARY_REPOSITORY_PATH/target/aarch64-apple-ios/release/libiroh_ble_chat_lib.a" "$EXTERNALS/libapp.a"

echo "==> Done. Toolchain versions:"
rustc --version
node --version
pnpm --version
ls -lh "$EXTERNALS/libapp.a"
