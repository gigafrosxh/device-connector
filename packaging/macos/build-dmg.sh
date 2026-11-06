#!/usr/bin/env bash
# Baut DeviceConnector.app (Universal: Apple Silicon + Intel) und ein .dmg.
# Aufruf: packaging/macos/build-dmg.sh [version]   → dist/DeviceConnector-<version>-macos-universal.dmg
set -euo pipefail
cd "$(dirname "$0")/../.."

VERSION="${1:-$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="device-connector"))')}"
SHORT="${VERSION%%-*}"

rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
cargo build --release -p device-connector --target aarch64-apple-darwin
cargo build --release -p device-connector --target x86_64-apple-darwin

APP="dist/Device Connector.app"
rm -rf dist && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create -output "$APP/Contents/MacOS/device-connector" \
  target/aarch64-apple-darwin/release/device-connector \
  target/x86_64-apple-darwin/release/device-connector
sed -e "s/__VERSION__/$VERSION/" -e "s/__SHORT_VERSION__/$SHORT/" packaging/macos/Info.plist > "$APP/Contents/Info.plist"

# Signatur. Ohne MACOS_SIGN_IDENTITY: ad hoc (Entwicklerkanal, Gatekeeper
# fragt nach, Berechtigungen müssen nach jedem Update neu erteilt werden).
# Mit Developer-ID: Hardened Runtime + Zeitstempel, danach Notarisierung.
IDENTITY="${MACOS_SIGN_IDENTITY:--}"
if [ "$IDENTITY" = "-" ]; then
  codesign --force --deep --options runtime --sign - "$APP"
else
  codesign --force --options runtime --timestamp --sign "$IDENTITY" "$APP/Contents/MacOS/device-connector"
  codesign --force --options runtime --timestamp --sign "$IDENTITY" "$APP"
  codesign --verify --strict --deep --verbose=2 "$APP"
fi

STAGE="dist/dmg"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Programme"
DMG="dist/DeviceConnector-$VERSION-macos-universal.dmg"
hdiutil create -volname "Device Connector" -srcfolder "$STAGE" -ov -format UDZO "$DMG"
rm -rf "$STAGE"

if [ "$IDENTITY" != "-" ]; then
  codesign --force --timestamp --sign "$IDENTITY" "$DMG"
  codesign --verify --verbose=2 "$DMG"
  if [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ] && [ -n "${APPLE_APP_PASSWORD:-}" ]; then
    # notarytool endet auch bei „Invalid“ mit 0, deshalb den Status prüfen.
    OUT="$(xcrun notarytool submit "$DMG" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" \
      --password "$APPLE_APP_PASSWORD" --wait --output-format json)"
    echo "$OUT"
    STATUS="$(printf '%s' "$OUT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("status",""))')"
    if [ "$STATUS" != "Accepted" ]; then
      ID="$(printf '%s' "$OUT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')"
      [ -n "$ID" ] && xcrun notarytool log "$ID" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_APP_PASSWORD" || true
      echo "Notarisierung fehlgeschlagen: $STATUS" >&2
      exit 1
    fi
    xcrun stapler staple "$DMG"
    xcrun stapler validate "$DMG"
    spctl --assess --type open --context context:primary-signature --verbose=2 "$DMG"
  elif [ "${REQUIRE_NOTARIZATION:-}" = "1" ]; then
    echo "Signiert, aber APPLE_ID/APPLE_TEAM_ID/APPLE_APP_PASSWORD fehlen" >&2
    exit 1
  else
    echo "Hinweis: signiert, aber nicht notarisiert (APPLE_ID usw. fehlen)." >&2
  fi
fi
echo "Fertig: $DMG"
