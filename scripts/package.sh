#!/bin/bash
# Build a universal AutoHarness.app and a distributable DMG.
#
#   ./scripts/package.sh                 # unsigned, for local testing
#   SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
#   NOTARY_PROFILE=autoharness \
#   RELEASE_TEAM_ID=TEAMID \
#   RELEASE_FEED_URL=https://releases.example.com/stable/appcast.json \
#     ./scripts/package.sh               # signed + notarized + stapled
#
# Signing and notarization are OPT-IN and never faked: without the variables
# above the script produces an honest unsigned bundle and says so, rather than
# emitting something that looks shippable and is not.
#
# `NOTARY_PROFILE` is a keychain profile created once with:
#   xcrun notarytool store-credentials autoharness \
#     --apple-id you@example.com --team-id TEAMID --password <app-specific>

set -euo pipefail

APP_NAME="AutoHarness"
BUNDLE_ID="dev.autoharness.app"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST="$ROOT/target/dist"
APP="$DIST/$APP_NAME.app"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
MIN_MACOS="14.0"
RELEASE_TEAM_ID="${RELEASE_TEAM_ID:-}"
RELEASE_FEED_URL="${RELEASE_FEED_URL:-}"

if [[ -n "$RELEASE_TEAM_ID" || -n "$RELEASE_FEED_URL" ]]; then
  if [[ ! "$RELEASE_TEAM_ID" =~ ^[A-Z0-9]{10}$ ]]; then
    echo "RELEASE_TEAM_ID must be exactly 10 uppercase letters or digits" >&2
    exit 1
  fi
  if [[ ! "$RELEASE_FEED_URL" =~ ^https://[^/@]+/.+\.json$ ]]; then
    echo "RELEASE_FEED_URL must be an HTTPS .json URL without credentials" >&2
    exit 1
  fi
fi

say() { printf '\033[1m==>\033[0m %s\n' "$1"; }

say "Building universal binaries (arm64 + x86_64) for $APP_NAME $VERSION"
for target in aarch64-apple-darwin x86_64-apple-darwin; do
  rustup target add "$target" >/dev/null 2>&1 || true
  AUTOHARNESS_RELEASE_TEAM_ID="$RELEASE_TEAM_ID" \
  AUTOHARNESS_RELEASE_FEED_URL="$RELEASE_FEED_URL" \
    cargo build --release --target "$target" \
      -p autoharness -p autoharnessd -p autoharness-updater
done

rm -rf "$DIST"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# The daemon ships beside the app binary: the UI launches it from there.
for binary in autoharness autoharnessd autoharness-updater; do
  lipo -create -output "$APP/Contents/MacOS/$binary" \
    "$ROOT/target/aarch64-apple-darwin/release/$binary" \
    "$ROOT/target/x86_64-apple-darwin/release/$binary"
done

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleExecutable</key><string>autoharness</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>$MIN_MACOS</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# The app spawns the daemon, which spawns the provider CLIs, so the hardened
# runtime needs the JIT/unsigned-memory allowances those Node/Rust binaries
# require. No network entitlement is granted to the app itself: egress belongs
# to the engine control processes and the brokered proxy.
cat > "$DIST/entitlements.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>com.apple.security.cs.allow-jit</key><true/>
  <key>com.apple.security.cs.allow-unsigned-executable-memory</key><true/>
  <key>com.apple.security.cs.disable-library-validation</key><true/>
</dict>
</plist>
PLIST

if [[ -n "${SIGN_IDENTITY:-}" ]]; then
  say "Signing with: $SIGN_IDENTITY"
  # Inner binaries first, bundle last — signatures nest.
  for binary in autoharnessd autoharness-updater; do
    codesign --force --timestamp --options runtime \
      --entitlements "$DIST/entitlements.plist" \
      --sign "$SIGN_IDENTITY" "$APP/Contents/MacOS/$binary"
  done
  codesign --force --timestamp --options runtime \
    --entitlements "$DIST/entitlements.plist" \
    --sign "$SIGN_IDENTITY" "$APP"
  codesign --verify --deep --strict --verbose=2 "$APP"
  ACTUAL_TEAM_ID="$(codesign -dv --verbose=4 "$APP" 2>&1 | sed -n 's/^TeamIdentifier=//p')"
  if [[ -n "$RELEASE_TEAM_ID" && "$ACTUAL_TEAM_ID" != "$RELEASE_TEAM_ID" ]]; then
    echo "Signed Team ID $ACTUAL_TEAM_ID does not match RELEASE_TEAM_ID $RELEASE_TEAM_ID" >&2
    exit 1
  fi
else
  say "No SIGN_IDENTITY: producing an UNSIGNED bundle (local testing only)"
fi

if [[ -n "${NOTARY_PROFILE:-}" && -n "${SIGN_IDENTITY:-}" ]]; then
  PRE_NOTARY_ZIP="$DIST/$APP_NAME-notary.zip"
  ditto -c -k --sequesterRsrc --keepParent "$APP" "$PRE_NOTARY_ZIP"
  say "Submitting the signed app to Apple for notarization"
  xcrun notarytool submit "$PRE_NOTARY_ZIP" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$APP"
  xcrun stapler validate "$APP"
  rm -f "$PRE_NOTARY_ZIP"
elif [[ -n "${SIGN_IDENTITY:-}" ]]; then
  say "Signed but NOT notarized (set NOTARY_PROFILE to notarize)"
else
  say "UNSIGNED and NOT notarized — Gatekeeper will refuse this on another Mac"
fi

UPDATE_ZIP="$DIST/$APP_NAME-$VERSION.zip"
say "Building updater archive $UPDATE_ZIP"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$UPDATE_ZIP"

DMG="$DIST/$APP_NAME-$VERSION.dmg"
say "Building $DMG"
hdiutil create -volname "$APP_NAME" -srcfolder "$APP" -ov -format UDZO "$DMG" >/dev/null

if [[ -n "${SIGN_IDENTITY:-}" ]]; then
  codesign --force --timestamp --sign "$SIGN_IDENTITY" "$DMG"
fi

if [[ -n "${NOTARY_PROFILE:-}" && -n "${SIGN_IDENTITY:-}" ]]; then
  say "Submitting the DMG to Apple for notarization"
  xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$DMG"
  xcrun stapler validate "$DMG"
  say "Notarized and stapled: $APP and $DMG"
fi

if [[ -n "$RELEASE_TEAM_ID" && -n "$RELEASE_FEED_URL" ]]; then
  SHA256="$(shasum -a 256 "$UPDATE_ZIP" | awk '{print $1}')"
  ASSET_BASE_URL="${RELEASE_ASSET_BASE_URL:-${RELEASE_FEED_URL%/*}}"
  ASSET_URL="$ASSET_BASE_URL/$(basename "$UPDATE_ZIP")"
  cat > "$DIST/appcast.template.json" <<JSON
{
  "releases": [
    {
      "version": "$VERSION",
      "minimum_macos": "$MIN_MACOS",
      "url": "$ASSET_URL",
      "sha256": "$SHA256",
      "bundle_id": "$BUNDLE_ID",
      "team_id": "$RELEASE_TEAM_ID"
    }
  ]
}
JSON
  say "Pinned feed template: $DIST/appcast.template.json"
else
  say "Release feed inputs absent: this build truthfully disables self-update"
fi

say "Done: $UPDATE_ZIP and $DMG"
