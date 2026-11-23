#!/usr/bin/env bash
# Builds build/Restowa.app from the SwiftPM package. Needs only the Xcode
# Command Line Tools. Signs ad-hoc unless CODESIGN_IDENTITY is set.
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

codesign --force --sign "${CODESIGN_IDENTITY:--}" "$app"
codesign --verify --strict "$app"

echo "$app"
