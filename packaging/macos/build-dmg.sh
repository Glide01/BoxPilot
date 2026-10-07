#!/usr/bin/env bash
# Package BoxPilot for macOS as a DMG holding BoxPilot.app.
#
#   packaging/macos/build-dmg.sh <release-binary> <sing-box-binary> <version> <arch> <out-dir>
#
# e.g. packaging/macos/build-dmg.sh \
#        target/aarch64-apple-darwin/release/box_pilot_gui \
#        release/sing-box 1.13.5 arm64 release
#
# <arch> is arm64 or x86_64 and only names the file: both binaries must
# already be built for it. Writes <out-dir>/BoxPilot-<version>-macos-<arch>.dmg.
# The bundle is assembled in a temporary directory and removed afterwards:
#
#   BoxPilot.app/Contents/Info.plist           packaging/macos/Info.plist,
#                                              versions filled in
#   BoxPilot.app/Contents/MacOS/BoxPilot       the BoxPilot release binary
#   BoxPilot.app/Contents/MacOS/sing-box       the bundled sing-box; BoxPilot
#                                              looks for it next to its own
#                                              executable
#   BoxPilot.app/Contents/Resources/BoxPilot.icns   from assets/icon.png
#
# The app is signed ad hoc (no Developer ID, so not notarized either): the
# inner sing-box first, then the bundle, which seals it. Gatekeeper still
# blocks the first open of a downloaded copy; the release notes say how to
# allow it (ADR 0005). The DMG holds the app and an /Applications link to
# drag it onto.
#
# macOS only: needs sips, iconutil, codesign, plutil and hdiutil (all part
# of the base system / Xcode command line tools).

set -euo pipefail

die() {
    echo "build-dmg: $*" >&2
    exit 1
}

[ "$#" -eq 5 ] || die "usage: $0 <release-binary> <sing-box-binary> <version> <arch> <out-dir>"

app_bin=$1
singbox_bin=$2
version=$3
arch=$4
out_dir=$5

[ -f "$app_bin" ] || die "release binary not found: $app_bin"
[ -f "$singbox_bin" ] || die "sing-box binary not found: $singbox_bin"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || die "not a valid version: '$version'"
case "$arch" in
    arm64 | x86_64) ;;
    *) die "arch must be arm64 or x86_64, not '$arch'" ;;
esac
for tool in sips iconutil codesign plutil hdiutil; do
    command -v "$tool" >/dev/null || die "$tool not found (this script runs on macOS)"
done

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$here/../.." && pwd)
icon="$repo_root/assets/icon.png"
[ -f "$icon" ] || die "icon not found: $icon"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

staging="$work/dmg"
app="$staging/BoxPilot.app"
contents="$app/Contents"
mkdir -p "$contents/MacOS" "$contents/Resources"
install -m 0755 "$app_bin" "$contents/MacOS/BoxPilot"
install -m 0755 "$singbox_bin" "$contents/MacOS/sing-box"

# CFBundleVersion takes numbers and dots only: drop a pre-release suffix.
sed -e "s/@VERSION@/$version/g" -e "s/@BUNDLE_VERSION@/${version%%-*}/g" \
    "$here/Info.plist" >"$contents/Info.plist"
plutil -lint "$contents/Info.plist" >/dev/null || die "Info.plist is not valid"
printf 'APPL????' >"$contents/PkgInfo"

# Every size an .icns holds. The source is 256 px, so 512 and up are
# upscaled; Finder only shows those in its largest views.
iconset="$work/BoxPilot.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$icon" --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" "$icon" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$contents/Resources/BoxPilot.icns"

# Inside out: a bundle's signature seals the code it holds, so sing-box is
# signed before the bundle (which signs BoxPilot itself as its main
# executable).
codesign --force --sign - "$contents/MacOS/sing-box"
codesign --force --sign - "$app"
codesign --verify --strict --verbose=2 "$app"

ln -s /Applications "$staging/Applications"

mkdir -p "$out_dir"
out="$out_dir/BoxPilot-$version-macos-$arch.dmg"
rm -f "$out"

# hdiutil now and then fails with "Resource busy" on CI runners; a retry
# gets past it.
for attempt in 1 2 3; do
    if hdiutil create -volname "BoxPilot" -srcfolder "$staging" \
        -fs HFS+ -format UDZO -ov "$out"; then
        break
    fi
    [ "$attempt" -lt 3 ] || die "hdiutil create failed"
    sleep 5
done

echo "Built $out ($(du -h "$out" | cut -f1))"
