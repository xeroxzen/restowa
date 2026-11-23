#!/usr/bin/env bash
# Builds build/Restowa.app from the SwiftPM package with the Xcode Command
# Line Tools alone.
#
# Signing:
#   default                  ad-hoc (local use only; Gatekeeper rejects it elsewhere)
#   CODESIGN_IDENTITY="Developer ID Application: Name (TEAMID)"
#                            hardened runtime + entitlements + secure timestamp,
#                            ready for notarize.sh
set -euo pipefail

cd "$(dirname "$0")"

swift build -c release --product Restowa
bin_dir="$(swift build -c release --show-bin-path)"

app="build/Restowa.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin_dir/Restowa" "$app/Contents/MacOS/Restowa"
cp Resources/Info.plist "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist" >/dev/null

identity="${CODESIGN_IDENTITY:--}"
if [[ "$identity" == "-" ]]; then
    codesign --force --sign - "$app"
else
    codesign --force --sign "$identity" --options runtime --timestamp \
        --entitlements Resources/Restowa.entitlements "$app"
fi
codesign --verify --strict --verbose=1 "$app"

echo "$app"
