#!/usr/bin/env bash
#
# Print the code-signing environment for local macOS builds, creating a
# self-signed "clip-llm-dev" identity in its own keychain on first use.
#
# An ad-hoc signature pins the Accessibility grant to the build's cdhash, so
# every rebuild needs a new grant. A bundle signed with one stable identity
# has the designated requirement `identifier "com.zekaizer.clip-llm" and
# certificate leaf = H"..."`, which every later build signed the same way
# satisfies: grant Accessibility once.
#
# Usage: eval "$(scripts/macos-dev-signing.sh)"
#        scripts/package-macos-app.sh ...      # picks up CODESIGN_*
#
# Running it again only unlocks the keychain. The keychain password is random
# and kept in ~/.config/clip-llm/dev-keychain-password (mode 600); the private
# key never leaves the keychain.
#
set -euo pipefail

NAME="clip-llm-dev"
KEYCHAIN="$HOME/Library/Keychains/$NAME.keychain-db"
PASSWORD_FILE="$HOME/.config/clip-llm/dev-keychain-password"

identity_hash() {
  security find-identity -p codesigning "$KEYCHAIN" 2>/dev/null |
    awk -v name="\"$NAME\"" '$3 == name { print $2; exit }'
}

if [ ! -e "$KEYCHAIN" ]; then
  mkdir -p "$(dirname "$PASSWORD_FILE")"
  (umask 077 && openssl rand -hex 24 > "$PASSWORD_FILE")
  security create-keychain -p "$(cat "$PASSWORD_FILE")" "$KEYCHAIN"
  # No auto-lock timeout: a build should not stop on a locked keychain mid-run.
  security set-keychain-settings "$KEYCHAIN"
fi
security unlock-keychain -p "$(cat "$PASSWORD_FILE")" "$KEYCHAIN"

if [ -z "$(identity_hash)" ]; then
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  cat > "$tmp/req.cnf" <<CNF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $NAME
[ext]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
CNF
  openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$tmp/req.cnf" \
    -keyout "$tmp/key.pem" -out "$tmp/cert.pem" 2>/dev/null
  openssl pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" -name "$NAME" \
    -out "$tmp/id.p12" -passout pass:import
  security import "$tmp/id.p12" -k "$KEYCHAIN" -P import -T /usr/bin/codesign >/dev/null
  # Lets codesign use the key without a GUI prompt (needed over SSH).
  security set-key-partition-list -S apple-tool:,apple: -s -k "$(cat "$PASSWORD_FILE")" "$KEYCHAIN" >/dev/null
fi

hash="$(identity_hash)"
[ -n "$hash" ] || { echo "no $NAME identity in $KEYCHAIN" >&2; exit 1; }
echo "export CODESIGN_IDENTITY=$hash"
echo "export CODESIGN_KEYCHAIN=$KEYCHAIN"
