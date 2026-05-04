#!/usr/bin/env bash
# Age-encrypted secrets helper.
#
# Commands:
#   encrypt <file>...   Encrypt plaintext files into .github/secrets/<name>.age
#                       against the public keys in recipients.txt.
#   decrypt             Decrypt every .age file into .secrets/ (gitignored).
#   rekey               Re-encrypt every .age file against the current
#                       recipients.txt (run after editing that file).
#
# Environment:
#   AGE_IDENTITY_FILE   Path to the age identity (private key).
#                       Default: $HOME/.config/age/iroh-ble-chat.txt
#
# New machine setup:
#   1. age-keygen -o ~/.config/age/iroh-ble-chat.txt
#   2. grep '^# public key:' ~/.config/age/iroh-ble-chat.txt \
#        | cut -d' ' -f4 >> .github/secrets/recipients.txt
#   3. scripts/secrets.sh rekey   (if other devs already have encrypted files)

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
SECRETS_DIR="$REPO_ROOT/.github/secrets"
RECIPIENTS="$SECRETS_DIR/recipients.txt"
LOCAL_DIR="$REPO_ROOT/.secrets"
DEFAULT_IDENTITY="$HOME/.config/age/iroh-ble-chat.txt"

die() { echo "error: $*" >&2; exit 1; }

need_age() {
  command -v age >/dev/null || die "'age' not found on PATH (mise: \`mise install age\`)"
}

identity_file() {
  local path="${AGE_IDENTITY_FILE:-$DEFAULT_IDENTITY}"
  [ -f "$path" ] || die "identity file not found: $path (set AGE_IDENTITY_FILE or create one with \`age-keygen -o $DEFAULT_IDENTITY\`)"
  printf '%s' "$path"
}

recipients_file() {
  [ -s "$RECIPIENTS" ] || die "recipients file missing or empty: $RECIPIENTS"
  printf '%s' "$RECIPIENTS"
}

cmd=${1:-}
shift || true

case "$cmd" in
  encrypt)
    need_age
    [ $# -gt 0 ] || die "usage: secrets.sh encrypt <plaintext-file>..."
    REC=$(recipients_file)
    mkdir -p "$SECRETS_DIR"
    for src in "$@"; do
      [ -f "$src" ] || die "not a file: $src"
      dest="$SECRETS_DIR/$(basename "$src").age"
      age --recipients-file "$REC" --output "$dest" "$src"
      echo "encrypted $src -> $dest"
    done
    ;;

  decrypt)
    need_age
    ID=$(identity_file)
    mkdir -p "$LOCAL_DIR"
    shopt -s nullglob
    files=("$SECRETS_DIR"/*.age)
    shopt -u nullglob
    [ ${#files[@]} -gt 0 ] || die "no .age files in $SECRETS_DIR"
    for f in "${files[@]}"; do
      out="$LOCAL_DIR/$(basename "${f%.age}")"
      age --decrypt --identity "$ID" --output "$out" "$f"
      echo "decrypted $(basename "$f") -> $out"
    done
    echo "done (files live in $LOCAL_DIR, which is gitignored)"
    ;;

  rekey)
    need_age
    ID=$(identity_file)
    REC=$(recipients_file)
    shopt -s nullglob
    files=("$SECRETS_DIR"/*.age)
    shopt -u nullglob
    [ ${#files[@]} -gt 0 ] || die "no .age files in $SECRETS_DIR"
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    for f in "${files[@]}"; do
      plain="$tmp/$(basename "${f%.age}")"
      age --decrypt --identity "$ID" --output "$plain" "$f"
      age --recipients-file "$REC" --output "$f" "$plain"
      echo "re-keyed $(basename "$f")"
    done
    ;;

  *)
    sed -n '2,/^set -euo/p' "$0" | sed -n '2,/^$/p' | sed 's/^# *//'
    exit 1
    ;;
esac
