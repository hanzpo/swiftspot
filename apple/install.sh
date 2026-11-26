#!/bin/bash
# Build SwiftSpot, the SwiftUI app with its Rust engine inside, and install
# it into /Applications.
#
#   apple/install.sh
#
# The app is one process: the SwiftUI front end with SwiftSpot's Rust
# engine, which keeps the Spotify session and plays the audio, linked in
# as a static library (src/ffi.rs). The app is com.hanzpo.swiftspot.
# The terminal client (tui/) goes in Contents/Helpers/swiftspot.
#
# The icon is SwiftSpot's own, Packaging/AppIcon.svg, rendered to
# Packaging/AppIcon-1024.png by Packaging/render-icon.swift; set ICON to
# another .icns file to use that instead. Set APP_DIR to
# install somewhere other than /Applications. The bundle is signed with
# this Mac's Apple Development certificate when it has one, or
# CODESIGN_IDENTITY when set ("-" signs ad hoc), with the hardened runtime
# that notarization asks for. A running SwiftSpot is quit
# first and the new one opened afterwards. BUILD_ONLY=1 builds and checks
# the bundle without installing it.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"

app_dir="${APP_DIR:-/Applications}"
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n1)"
if [ -z "${CODESIGN_IDENTITY:-}" ]; then
    CODESIGN_IDENTITY="$(security find-identity -v -p codesigning \
        | sed -n 's/^ *[0-9]*) \([0-9A-F]\{40\}\) "Apple Development: .*"$/\1/p' \
        | head -n1)"
fi
identity="${CODESIGN_IDENTITY:--}"
sign=(codesign --force --options runtime --sign "$identity")
[ "$identity" = - ] || sign+=(--timestamp)

if [ -z "${ICON:-}" ]; then
    iconset="$(mktemp -d)/AppIcon.iconset"
    mkdir -p "$iconset"
    for size in 16 32 128 256 512; do
        double=$((size * 2))
        sips -z $size $size "$here/Packaging/AppIcon-1024.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
        sips -z $double $double "$here/Packaging/AppIcon-1024.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
    done
    ICON="${iconset%.iconset}.icns"
    iconutil -c icns "$iconset" -o "$ICON"
fi

(cd "$root" && cargo build --release --locked --lib && cargo build --release --locked -p swiftspot-tui)
(cd "$here" && swift build -c release)
bundle="$here/.build/SwiftSpot.app"
rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp "$(cd "$here" && swift build -c release --show-bin-path)/SwiftSpot" "$bundle/Contents/MacOS/SwiftSpot"
sed "s/__VERSION__/$version/g" "$here/Packaging/Info.plist" > "$bundle/Contents/Info.plist"
cp "$ICON" "$bundle/Contents/Resources/AppIcon.icns"
mkdir -p "$bundle/Contents/Helpers"
cp "$root/target/release/swiftspot" "$bundle/Contents/Helpers/swiftspot"
"${sign[@]}" "$bundle/Contents/Helpers/swiftspot"
"${sign[@]}" "$bundle"
codesign --verify --deep --strict "$bundle"

# BUILD_ONLY stops here, with the bundle built and checked.
if [ -n "${BUILD_ONLY:-}" ]; then
    echo "$bundle"
    exit 0
fi

# Quit what runs now: this app, and the app under its older names,
# Spotlite and Spotifast.
for name in SwiftSpot Spotlite Spotifast; do
    if pgrep -xq "$name"; then
        osascript -e "quit app \"$name\"" 2>/dev/null || true
        for _ in $(seq 20); do
            pgrep -xq "$name" || break
            sleep 0.5
        done
        pkill -x "$name" 2>/dev/null || true
    fi
done

rm -rf "$app_dir/SwiftSpot.app" "$app_dir/Spotlite.app" "$app_dir/Spotifast.app"
cp -R "$bundle" "$app_dir/"
codesign --verify --deep --strict "$app_dir/SwiftSpot.app"
open "$app_dir/SwiftSpot.app"
echo "Installed SwiftSpot $version in $app_dir"
