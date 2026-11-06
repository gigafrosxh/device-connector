#!/usr/bin/env bash
# CI: Developer-ID-Zertifikat aus Secrets in einen temporären Schlüsselbund
# laden, damit codesign es findet. Braucht MACOS_CERT_P12 (base64) und
# MACOS_CERT_PASSWORD. Lokal nicht nötig: dort liegt das Zertifikat im
# Anmelde-Schlüsselbund.
set -euo pipefail
: "${MACOS_CERT_P12:?fehlt}" "${MACOS_CERT_PASSWORD:?fehlt}"
TMP="${RUNNER_TEMP:-$(mktemp -d)}"
KEYCHAIN="$TMP/dc-sign.keychain-db"
PW="$(uuidgen)"

security create-keychain -p "$PW" "$KEYCHAIN"
security set-keychain-settings -lut 21600 "$KEYCHAIN"
security unlock-keychain -p "$PW" "$KEYCHAIN"
printf '%s' "$MACOS_CERT_P12" | base64 --decode > "$TMP/dc-cert.p12"
security import "$TMP/dc-cert.p12" -P "$MACOS_CERT_PASSWORD" -A -t cert -f pkcs12 -k "$KEYCHAIN"
rm -f "$TMP/dc-cert.p12"
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$PW" "$KEYCHAIN" >/dev/null
# shellcheck disable=SC2046
security list-keychains -d user -s "$KEYCHAIN" $(security list-keychains -d user | tr -d '"')
security find-identity -v -p codesigning "$KEYCHAIN"
