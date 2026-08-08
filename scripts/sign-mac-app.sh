#!/usr/bin/env sh
set -eu

CERT_NAME="${MYKVM_CODESIGN_IDENTITY:-MyKVM Local Code Signing}"
KEYCHAIN="${MYKVM_CODESIGN_KEYCHAIN:-$HOME/Library/Keychains/mykvm-local-signing.keychain-db}"
KEYCHAIN_PASSWORD="${MYKVM_CODESIGN_KEYCHAIN_PASSWORD:-mykvm-local}"
P12_PASSWORD="${MYKVM_CODESIGN_P12_PASSWORD:-mykvm-local}"
APP_PATH="${1:-/Applications/mykvm.app}"
DMG_PATH="${2:-}"

if [ "$(uname -s)" != "Darwin" ]; then
  printf "macOS signing must run on macOS.\n" >&2
  exit 1
fi

if [ ! -d "$APP_PATH" ]; then
  printf "App bundle not found: %s\n" "$APP_PATH" >&2
  exit 1
fi

ensure_local_identity() {
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM
  created_identity=0

  if [ ! -f "$KEYCHAIN" ]; then
    security create-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
  fi
  security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
  security set-keychain-settings -lut 21600 "$KEYCHAIN"

  current_keychains="$(security list-keychains -d user | sed 's/[" ]//g')"
  if ! printf '%s\n' "$current_keychains" | grep -Fxq "$KEYCHAIN"; then
    # shellcheck disable=SC2086
    security list-keychains -d user -s "$KEYCHAIN" $current_keychains
  fi

  if ! security find-certificate -c "$CERT_NAME" "$KEYCHAIN" >/dev/null 2>&1; then
    cat > "$tmp_dir/openssl.cnf" <<EOF
[ req ]
prompt = no
distinguished_name = dn
x509_extensions = codesign_ext

[ dn ]
CN = $CERT_NAME

[ codesign_ext ]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
subjectKeyIdentifier = hash
EOF

    openssl req -new -newkey rsa:2048 -nodes -x509 -days 3650 \
      -config "$tmp_dir/openssl.cnf" \
      -keyout "$tmp_dir/mykvm-codesign.key" \
      -out "$tmp_dir/mykvm-codesign.crt" >/dev/null 2>&1

    # 3DES/SHA1 p12 encryption keeps the output importable by macOS security.
    openssl pkcs12 -export \
      -inkey "$tmp_dir/mykvm-codesign.key" \
      -in "$tmp_dir/mykvm-codesign.crt" \
      -name "$CERT_NAME" \
      -out "$tmp_dir/mykvm-codesign.p12" \
      -passout "pass:$P12_PASSWORD" \
      -keypbe PBE-SHA1-3DES \
      -certpbe PBE-SHA1-3DES \
      -macalg sha1 >/dev/null 2>&1

    security import "$tmp_dir/mykvm-codesign.p12" \
      -k "$KEYCHAIN" \
      -P "$P12_PASSWORD" \
      -T /usr/bin/codesign \
      -T /usr/bin/security >/dev/null
    created_identity=1
  fi

  # Best effort: mark the cert trusted for code signing. A user-domain trust is
  # enough for some operations, but macOS only reports it as a *valid*
  # codesigning identity once it lives in the System keychain trusted roots
  # (which needs `sudo`). We don't fail if this can't be done headlessly — the
  # caller falls back to ad-hoc signing below.
  if [ "$created_identity" -eq 1 ]; then
    security find-certificate -c "$CERT_NAME" -p "$KEYCHAIN" > "$tmp_dir/mykvm-codesign.crt"
    security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" \
      "$tmp_dir/mykvm-codesign.crt" >/dev/null 2>&1 || true
  fi

  security set-key-partition-list -S apple-tool:,apple:,codesign: \
    -s -k "$KEYCHAIN_PASSWORD" "$KEYCHAIN" >/dev/null
}

ensure_local_identity

# Choose the signing identity:
#  - Prefer the self-signed cert IF it is a *valid* (trusted) codesigning
#    identity. A proper cert preserves TCC grants (Accessibility / Input
#    Monitoring) across updates and opens on any Mac that trusts it.
#  - Otherwise fall back to ad-hoc (`-`). This still produces a valid signature
#    and, once the quarantine flag is stripped, lets the app open without the
#    "unidentified developer" Gatekeeper block on the local machine.
SIGN_IDENTITY="-"
if security find-identity -v -p codesigning 2>/dev/null | grep -F "\"$CERT_NAME\"" >/dev/null; then
  SIGN_IDENTITY="$CERT_NAME"
  echo "Using trusted self-signed identity: $CERT_NAME"
else
  echo "Self-signed cert is not a trusted codesigning identity; using ad-hoc signing."
  echo "To enable a trusted cert (recommended for updates + cross-machine use), run once with sudo:"
  echo "  sudo security add-trusted-cert -d -r trustRoot -p codeSign -k /Library/Keychains/System.keychain \"$KEYCHAIN\""
fi

xattr -dr com.apple.quarantine "$APP_PATH" 2>/dev/null || true
codesign --force --deep --sign "$SIGN_IDENTITY" --identifier com.xzhpl.mykvm "$APP_PATH" \
  || { echo "codesign failed" >&2; exit 1; }
codesign --verify --deep --verbose=2 "$APP_PATH" || true
codesign -dr - "$APP_PATH" 2>&1 || true

if [ -n "$DMG_PATH" ] && [ -f "$DMG_PATH" ]; then
  xattr -dr com.apple.quarantine "$DMG_PATH" 2>/dev/null || true
  codesign --force --sign "$SIGN_IDENTITY" "$DMG_PATH" \
    || { echo "dmg codesign failed" >&2; exit 1; }
  codesign --verify --verbose=2 "$DMG_PATH" || true
  echo "Signed disk image: $DMG_PATH"
fi

echo "Signed with identity: $SIGN_IDENTITY"
