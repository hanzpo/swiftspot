#!/bin/bash
# Build, notarize and publish a SwiftSpot release, then point the Homebrew
# cask in hanzpo/homebrew-tap at it, so `brew install hanzpo/tap/swiftspot`
# installs it.
#
#   apple/release.sh
#
# The version is Cargo.toml's and packaging/release-notes/vVERSION.md is the
# release's description. HEAD has to be on origin/main. Signing needs a
# Developer ID Application certificate in the keychain, and notarization a
# notarytool profile, swiftspot unless NOTARY_PROFILE names another:
#
#   xcrun notarytool store-credentials swiftspot --apple-id ID --team-id TEAM
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"

version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n1)"
tag="v$version"
notes="$root/packaging/release-notes/$tag.md"
profile="${NOTARY_PROFILE:-swiftspot}"

[ -f "$notes" ] || { echo "Write $notes first." >&2; exit 1; }
git -C "$root" fetch -q origin main
git -C "$root" merge-base --is-ancestor HEAD origin/main \
    || { echo "Push HEAD to origin/main first." >&2; exit 1; }
[ -z "$(git -C "$root" status --porcelain)" ] \
    || { echo "Commit or stash the working tree first." >&2; exit 1; }

identity="$(security find-identity -v -p codesigning \
    | sed -n 's/^ *[0-9]*) \([0-9A-F]\{40\}\) "Developer ID Application: .*"$/\1/p' \
    | head -n1)"
[ -n "$identity" ] || { echo "No Developer ID Application certificate in the keychain." >&2; exit 1; }
xcrun notarytool history --keychain-profile "$profile" >/dev/null

bundle="$(CODESIGN_IDENTITY="$identity" BUILD_ONLY=1 "$here/install.sh" | tail -n1)"

out="$root/dist/$tag"
zip="$out/SwiftSpot-$version.zip"
rm -rf "$out"
mkdir -p "$out"
ditto -c -k --keepParent "$bundle" "$zip"
result="$(xcrun notarytool submit "$zip" --keychain-profile "$profile" --wait --output-format json)"
status="$(plutil -extract status raw - <<<"$result")"
if [ "$status" != Accepted ]; then
    id="$(plutil -extract id raw - <<<"$result")"
    xcrun notarytool log "$id" --keychain-profile "$profile" >&2
    exit 1
fi
xcrun stapler staple "$bundle"
spctl --assess --type execute "$bundle"
rm "$zip"
ditto -c -k --keepParent "$bundle" "$zip"
sha="$(shasum -a 256 "$zip" | cut -d' ' -f1)"

gh release create "$tag" "$zip" --repo hanzpo/swiftspot \
    --target "$(git -C "$root" rev-parse HEAD)" \
    --title "SwiftSpot $version" --notes-file "$notes"

tap="$(mktemp -d)/homebrew-tap"
trap 'rm -rf "$(dirname "$tap")"' EXIT
gh repo clone hanzpo/homebrew-tap "$tap" -- -q
mkdir -p "$tap/Casks"
sed -e "s/@VERSION@/$version/" -e "s/@MACOS_SHA256@/$sha/" \
    "$root/packaging/homebrew/swiftspot.rb.in" > "$tap/Casks/swiftspot.rb"
git -C "$tap" add Casks/swiftspot.rb
git -C "$tap" commit -qm "swiftspot $version"
git -C "$tap" push -q

rm -rf "$out"
echo "Released SwiftSpot $version: brew install hanzpo/tap/swiftspot"
