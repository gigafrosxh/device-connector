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

# Ad-hoc-Signatur (ohne Apple-Entwicklerkonto). Mit Konto: MACOS_SIGN_IDENTITY setzen.
codesign --force --deep --options runtime --sign "${MACOS_SIGN_IDENTITY:--}" "$APP"

STAGE="dist/dmg"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Programme"
DMG="dist/DeviceConnector-$VERSION-macos-universal.dmg"
hdiutil create -volname "Device Connector" -srcfolder "$STAGE" -ov -format UDZO "$DMG"
rm -rf "$STAGE"
echo "Fertig: $DMG"
