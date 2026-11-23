#!/usr/bin/env bash
# Builds a Developer ID signed Restowa.app, notarises it, staples the ticket
# and writes build/Restowa-<version>.zip for a GitHub release.
#
# One-time setup:
#   1. Install a "Developer ID Application" certificate in your login keychain.
#   2. xcrun notarytool store-credentials restowa-notary \
#        --apple-id <apple id> --team-id <TEAMID> --password <app-specific password>
#
# Run:
#   CODESIGN_IDENTITY="Developer ID Application: Name (TEAMID)" ./notarize.sh
#   NOTARY_PROFILE overrides the keychain profile name (default restowa-notary).
set -euo pipefail

cd "$(dirname "$0")"

: "${CODESIGN_IDENTITY:?set CODESIGN_IDENTITY to a Developer ID Application identity}"
if [[ "$CODESIGN_IDENTITY" == "-" ]]; then
    echo "error: ad-hoc signatures cannot be notarised" >&2
    exit 1
fi
profile="${NOTARY_PROFILE:-restowa-notary}"

./build-app.sh
app="build/Restowa.app"
version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist")"
upload="build/Restowa-upload.zip"
zip="build/Restowa-${version}.zip"

ditto -c -k --keepParent "$app" "$upload"
xcrun notarytool submit "$upload" --keychain-profile "$profile" --wait
rm -f "$upload"

xcrun stapler staple "$app"
xcrun stapler validate "$app"
spctl --assess --type execute --verbose=2 "$app"

rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"
echo "$zip"
